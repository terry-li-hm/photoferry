#![allow(dead_code)]

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use swift_rs::{Bool, SRString, swift};

// MARK: - FFI declarations

swift!(fn photoferry_check_access() -> SRString);
swift!(fn photoferry_import_photo(path: &SRString, metadata_json: &SRString) -> SRString);
swift!(fn photoferry_create_album(title: &SRString) -> SRString);
swift!(fn photoferry_add_to_album(album_id: &SRString, asset_id: &SRString) -> Bool);

// MARK: - Types

#[derive(Debug, Serialize, Deserialize)]
pub struct PhotoMetadata {
    #[serde(rename = "creationDate", skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "isFavorite", skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ImportResult {
    pub success: bool,
    #[serde(rename = "localIdentifier")]
    pub local_identifier: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AccessResult {
    pub authorized: bool,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct AlbumResult {
    album_id: Option<String>,
    error: Option<String>,
}

// MARK: - Public API

pub fn check_access() -> Result<AccessResult> {
    let json = unsafe { photoferry_check_access() };
    let result: AccessResult = serde_json::from_str(json.as_str())?;
    Ok(result)
}

pub fn import_photo(path: &str, metadata: Option<&PhotoMetadata>) -> Result<ImportResult> {
    let path_sr: SRString = path.into();
    let meta_json = match metadata {
        Some(m) => serde_json::to_string(m)?,
        None => String::new(),
    };
    let meta_sr: SRString = meta_json.as_str().into();

    let json = unsafe { photoferry_import_photo(&path_sr, &meta_sr) };
    let result: ImportResult = serde_json::from_str(json.as_str())?;
    Ok(result)
}

pub fn create_album(title: &str) -> Result<String> {
    let title_sr: SRString = title.into();
    let json = unsafe { photoferry_create_album(&title_sr) };
    let result: AlbumResult = serde_json::from_str(json.as_str())?;

    if let Some(err) = result.error {
        bail!("Failed to create album: {}", err);
    }
    result
        .album_id
        .ok_or_else(|| anyhow::anyhow!("No album ID returned"))
}

pub fn add_to_album(album_id: &str, asset_id: &str) -> Result<bool> {
    let album_sr: SRString = album_id.into();
    let asset_sr: SRString = asset_id.into();
    let success: Bool = unsafe { photoferry_add_to_album(&album_sr, &asset_sr) };
    Ok(success.into())
}
