//! Contains helper structs to parse the pack metadata file for information relevant for
//! optimization purposes.

use std::io;
use std::path::Path;

use enumset::EnumSet;
use json_comments::StripComments;
use serde_json::Value;
use thiserror::Error;
use tokio::io::AsyncReadExt;

use crate::pack_file::asset_type::PackFileAssetType;
use crate::pack_file::strip_utf8_bom;
use crate::{config::MinecraftQuirk, vfs::VirtualFileSystem};

#[cfg(test)]
mod tests;

/// The pack format version used in Minecraft versions from 1.13 to 1.14.4.
pub const PACK_FORMAT_VERSION_1_13: i32 = 4;
/// The pack format version used in Minecraft versions from 1.15 to 1.16.1.
pub const PACK_FORMAT_VERSION_1_15: i32 = 5;
/// The pack format version used in Minecraft versions from 1.17 to 1.17.1.
pub const PACK_FORMAT_VERSION_1_17: i32 = 7;
/// The resource pack format version used in Minecraft versions from 23w17a to 1.20.1.
pub const PACK_FORMAT_RESOURCE_PACK_VERSION_23W_17A: i32 = 15;
/// The resource pack format version used in Minecraft versions from 21w39a (1.18 snapshot)
/// to 1.18.2.
pub const PACK_FORMAT_RESOURCE_PACK_VERSION_1_18: i32 = 8;
/// The resource pack format version used in Minecraft versions from 24w13a (1.20.5 snapshot)
/// to 1.20.5-pre3.
pub const PACK_FORMAT_RESOURCE_PACK_VERSION_24W_13A: i32 = 31;
/// The resource pack format version used in Minecraft version 24w40a (1.21.2 snapshot).
pub const PACK_FORMAT_RESOURCE_PACK_VERSION_24W_40A: i32 = 40;
/// The data pack format version used in Minecraft versions from 24w21a (1.21 snapshot)
/// to 1.21-pre1.
pub const PACK_FORMAT_DATA_PACK_VERSION_24W_21A: i32 = 45;

/// Metadata for a resource or data pack, contained in the `pack.mcmeta` or
/// `pack.mcmetac` file in the root folder of a pack.
///
/// Since Minecraft 25w31a (resource pack format 65, data pack format 82), a pack
/// no longer declares a single "main" supported format: it declares an inclusive
/// *range* of supported format versions, so one pack ZIP can target several game
/// versions at once. We model that range here so the quirk and asset-type logic
/// can reason about every Minecraft version a pack targets. The range is taken
/// from, in order of precedence, `min_format`/`max_format` (the modern scheme),
/// `supported_formats` (an intermediate scheme), or a single `pack_format` (the
/// oldest scheme). When a pack declares a single version, `min == max` and the
/// logic below reduces exactly to the historical single-`pack_format` behaviour.
///
/// References:
/// - <https://minecraft.wiki/w/Pack_format>
/// - <https://minecraft.wiki/w/Resource_Pack#Contents>
/// - <https://minecraft.wiki/w/Data_Pack#pack.mcmeta>
/// - Minecraft classes `net.minecraft.server.packs.metadata.pack.PackMetadataSection`
///   and `net.minecraft.server.packs.metadata.pack.PackFormat`
pub struct PackMeta {
	/// The major component of the lowest pack format version the pack targets.
	min_format_version: i32,
	/// The major component of the highest pack format version the pack targets.
	max_format_version: i32
}

/// Represents an error that may happen while parsing pack metadata files.
#[derive(Error, Debug)]
pub enum PackMetaError {
	#[error("JSON error: {0}")]
	JsonSerde(#[from] serde_json::Error),
	#[error("Syntax error: {0}")]
	MalformedMeta(&'static str),
	#[error("I/O error: {0}")]
	Io(#[from] io::Error)
}

/// Reads the major component of a pack format version field.
///
/// A format version is serialized either as a single JSON number or, since
/// 25w31a, as an array whose first element is the major version and whose
/// optional second element is the minor version (irrelevant for our major-based
/// logic). Minecraft first reads any value as a JSON number and then truncates
/// it to a 32-bit signed integer, so e.g. `42.8` is read as `42`; we mirror that
/// here with a saturating `as i32` cast.
fn format_major_version(value: &Value) -> Option<i32> {
	match value {
		Value::Number(number) => Some(number.as_f64()? as i32),
		Value::Array(array) => Some(array.first()?.as_f64()? as i32),
		_ => None
	}
}

/// Parses a `supported_formats` field into an inclusive `(min, max)` major
/// version range. Minecraft accepts a single integer (equivalent to a range with
/// both ends equal), a two-element `[min, max]` array, or an object of the form
/// `{ "min_inclusive": .., "max_inclusive": .. }`.
fn parse_supported_formats(value: &Value) -> Result<(i32, i32), PackMetaError> {
	const MALFORMED_SUPPORTED_FORMATS: &str =
		"\"supported_formats\" is not an integer, a [min, max] array or a \
		 { min_inclusive, max_inclusive } object";

	match value {
		Value::Number(number) => {
			let version =
				number.as_f64().ok_or(PackMetaError::MalformedMeta(MALFORMED_SUPPORTED_FORMATS))?
					as i32;
			Ok((version, version))
		}
		Value::Array(array) => {
			let min = array
				.first()
				.and_then(Value::as_f64)
				.ok_or(PackMetaError::MalformedMeta(MALFORMED_SUPPORTED_FORMATS))? as i32;
			let max = array.get(1).and_then(Value::as_f64).map_or(min, |max| max as i32);
			Ok((min.min(max), min.max(max)))
		}
		Value::Object(object) => {
			let min = object
				.get("min_inclusive")
				.and_then(Value::as_f64)
				.ok_or(PackMetaError::MalformedMeta(MALFORMED_SUPPORTED_FORMATS))? as i32;
			let max = object
				.get("max_inclusive")
				.and_then(Value::as_f64)
				.ok_or(PackMetaError::MalformedMeta(MALFORMED_SUPPORTED_FORMATS))? as i32;
			Ok((min.min(max), min.max(max)))
		}
		_ => Err(PackMetaError::MalformedMeta(MALFORMED_SUPPORTED_FORMATS))
	}
}

impl PackMeta {
	/// Creates a new pack metadata struct from a virtual filesystem and its root path.
	pub async fn new(
		vfs: &impl VirtualFileSystem,
		root_path: impl AsRef<Path>
	) -> Result<Self, PackMetaError> {
		const MALFORMED_FORMAT_VERSION: &str =
			"A pack format version is not an integer or a [major, minor] array";

		let min_format_version;
		let max_format_version;

		let mut file = vfs
			.open(root_path.as_ref().join("pack.mcmetac"))
			.or_else(|_| vfs.open(root_path.as_ref().join("pack.mcmeta")))?;
		let mut pack_meta_value =
			Vec::with_capacity(file.file_size_hint.try_into().unwrap_or(usize::MAX));

		file.file_read.read_to_end(&mut pack_meta_value).await?;

		// Parse the pack metadata to get its format version range and do some basic
		// validation. We do this parsing manually, instead of using auxiliary structs
		// that derive deserialization traits, because it is faster, provides more
		// relevant error information, and we only need to parse a few things that are
		// unlikely to change
		match serde_json::from_reader(StripComments::new(strip_utf8_bom(&pack_meta_value)))? {
			Value::Object(root_object) => {
				match root_object.get("pack").ok_or(PackMetaError::MalformedMeta(
					"Missing \"pack\" key in root object"
				))? {
					Value::Object(pack_meta_object) => {
						// Determine the inclusive range of pack format versions the pack
						// targets, mirroring how the vanilla game decides it. The modern
						// scheme (25w31a+) uses min_format/max_format; an intermediate one
						// uses supported_formats; the oldest uses a single pack_format.
						if let Some(min_format) = pack_meta_object.get("min_format") {
							min_format_version = format_major_version(min_format).ok_or(
								PackMetaError::MalformedMeta(MALFORMED_FORMAT_VERSION)
							)?;
							// When min_format is present the game requires max_format too;
							// treat an absent one as "unbounded above" to stay lenient.
							max_format_version = match pack_meta_object.get("max_format") {
								Some(max_format) => format_major_version(max_format).ok_or(
									PackMetaError::MalformedMeta(MALFORMED_FORMAT_VERSION)
								)?,
								None => i32::MAX
							};
						} else if let Some(supported_formats) =
							pack_meta_object.get("supported_formats")
						{
							let (min, max) = parse_supported_formats(supported_formats)?;
							min_format_version = min;
							max_format_version = max;
						} else if let Some(pack_format) = pack_meta_object.get("pack_format") {
							let version = format_major_version(pack_format).ok_or(
								PackMetaError::MalformedMeta(MALFORMED_FORMAT_VERSION)
							)?;
							min_format_version = version;
							max_format_version = version;
						} else {
							return Err(PackMetaError::MalformedMeta(
								"Missing \"pack_format\", \"min_format\" or \
								 \"supported_formats\" key in pack metadata object"
							));
						}

						// Also validate the pack description, because it is required by Minecraft
						match pack_meta_object.get("description") {
							Some(Value::String(_))
							| Some(Value::Object(_))
							| Some(Value::Array(_)) => {
								// This can possibly be a Minecraft text component, parsed by the
								// static class Serializer at net.minecraft.network.chat.Component
							}
							Some(_) => {
								return Err(PackMetaError::MalformedMeta(
									"The \"description\" key value is not a text component"
								));
							}
							None => {
								return Err(PackMetaError::MalformedMeta(
									"Missing \"description\" key in pack metadata object"
								));
							}
						};
					}
					_ => {
						return Err(PackMetaError::MalformedMeta(
							"The \"pack\" key value is not a JSON object"
						));
					}
				}
			}
			_ => {
				return Err(PackMetaError::MalformedMeta(
					"The JSON value is not an object"
				));
			}
		};

		Ok(Self {
			min_format_version,
			max_format_version
		})
	}

	/// Returns a maybe pessimistic set of Minecraft quirks that will need to be
	/// worked around to guarantee that the pack will work as expected.
	///
	/// This is done by looking at the range of pack format versions declared in
	/// the pack metadata, which specifies the Minecraft versions the pack is meant
	/// to be compatible with. A quirk is returned if *any* targeted version is (or
	/// may be) affected by it. For a single-version pack (`min == max`) this is
	/// exactly the historical single-`pack_format` behaviour.
	pub fn target_minecraft_versions_quirks(&self) -> EnumSet<MinecraftQuirk> {
		let mut quirks = EnumSet::empty();
		let min = self.min_format_version;
		let max = self.max_format_version;

		if min < PACK_FORMAT_VERSION_1_13 {
			quirks |= MinecraftQuirk::GrayscaleImagesGammaMiscorrection;
			quirks |= MinecraftQuirk::RestrictiveBannerLayerTextureFormatCheck;
			quirks |= MinecraftQuirk::PngObfuscationIncompatibility;
		}

		if min < PACK_FORMAT_VERSION_1_15 || max >= PACK_FORMAT_RESOURCE_PACK_VERSION_24W_13A {
			// Minecraft 1.14 is compatible with this feature, but we can't tell
			// it apart from 1.13 due to it sharing the same version number, so
			// err on the safe side. For the time being, 24w14a is the last version
			// to support this feature, but it shares a version number with 24w13a
			quirks |= MinecraftQuirk::OggObfuscationIncompatibility;
		}

		if min < PACK_FORMAT_VERSION_1_17 {
			quirks |= MinecraftQuirk::Java8ZipParsing;
		}

		if min < PACK_FORMAT_RESOURCE_PACK_VERSION_24W_40A {
			// 24w39a is the first snapshot to have this fixed, but we can't tell it
			// apart from 24w38a due to it sharing the same pack format version number,
			// so err on the safe side
			quirks |= MinecraftQuirk::BadEntityEyeLayerTextureTransparencyBlending;
		}

		quirks
	}

	/// Returns a maybe pessimistic set of pack file asset types that Minecraft and
	/// its mods can read from a pack.
	///
	/// This is done by looking at the range of pack format versions declared in
	/// the pack metadata, which specifies the Minecraft versions the pack is meant
	/// to be compatible with. An asset type is kept whenever *any* targeted version
	/// may use it, and only removed when *no* targeted version does. For a
	/// single-version pack (`min == max`) this is exactly the historical
	/// single-`pack_format` behaviour.
	pub fn target_minecraft_version_asset_type_mask(&self) -> EnumSet<PackFileAssetType> {
		let mut asset_type_mask = EnumSet::all();
		let min = self.min_format_version;
		let max = self.max_format_version;

		if min >= PACK_FORMAT_VERSION_1_13 {
			asset_type_mask -= PackFileAssetType::LegacyLanguageFile;
			asset_type_mask -= PackFileAssetType::TrueTypeFont;
		}

		if min >= PACK_FORMAT_VERSION_1_17 {
			asset_type_mask -= PackFileAssetType::LegacyTextCredits;
		}
		if max < PACK_FORMAT_VERSION_1_17 {
			asset_type_mask -= PackFileAssetType::TranslationUnitSegment;
		}

		if max < PACK_FORMAT_RESOURCE_PACK_VERSION_1_18 {
			asset_type_mask -= PackFileAssetType::ClosingCreditsText;
		}

		if min >= PACK_FORMAT_RESOURCE_PACK_VERSION_23W_17A {
			asset_type_mask -= PackFileAssetType::LegacyUnicodeFontCharacterSizes;
		}

		if min >= PACK_FORMAT_DATA_PACK_VERSION_24W_21A {
			asset_type_mask -= PackFileAssetType::LegacyNbtStructure;
			asset_type_mask -= PackFileAssetType::LegacyCommandFunction;
		}

		asset_type_mask
	}
}
