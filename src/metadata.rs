use anyhow::Result;
use chrono::DateTime;
use serde::Deserialize;

use crate::importer::PhotoMetadata;

// MARK: - Google Takeout sidecar JSON structures

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct TakeoutJson {
    pub title: Option<String>,
    pub description: Option<String>,
    pub photo_taken_time: Option<TimestampField>,
    pub geo_data_exif: Option<GeoData>,
    pub geo_data: Option<GeoData>,
    pub favorited: Option<bool>,
    pub trashed: Option<bool>,
    pub archived: Option<bool>,
    pub people: Option<Vec<Person>>,
    pub album_data: Option<AlbumData>,
}

#[derive(Debug, Deserialize)]
pub struct TimestampField {
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct GeoData {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Person {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct AlbumData {
    pub title: String,
}

// MARK: - Parsing

pub fn parse_sidecar(json_bytes: &[u8]) -> Result<TakeoutJson> {
    let parsed: TakeoutJson = serde_json::from_slice(json_bytes)?;
    Ok(parsed)
}

// MARK: - Conversion to PhotoMetadata

impl TakeoutJson {
    pub fn to_photo_metadata(&self) -> PhotoMetadata {
        PhotoMetadata {
            creation_date: self.parse_timestamp(),
            latitude: self.best_latitude(),
            longitude: self.best_longitude(),
            altitude: self.best_altitude(),
            title: self.title.clone(),
            description: self.description.clone(),
            is_favorite: Some(self.favorited.unwrap_or(false)),
        }
    }

    pub fn is_trashed(&self) -> bool {
        self.trashed.unwrap_or(false)
    }

    fn parse_timestamp(&self) -> Option<String> {
        let ts_str = self.photo_taken_time.as_ref()?.timestamp.as_str();

        // Empty or zero = no timestamp
        if ts_str.is_empty() || ts_str == "0" {
            return None;
        }

        let epoch: i64 = ts_str.parse().ok()?;
        if epoch == 0 {
            return None;
        }

        // Handle negative timestamps (pre-1970) and positive
        let dt = DateTime::from_timestamp(epoch, 0)?;
        Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    }

    fn best_geo(&self) -> Option<&GeoData> {
        // Prefer geo_data_exif, fallback to geo_data, skip (0.0, 0.0)
        self.geo_data_exif
            .as_ref()
            .filter(|g| !is_zero_gps(g))
            .or_else(|| self.geo_data.as_ref().filter(|g| !is_zero_gps(g)))
    }

    fn best_latitude(&self) -> Option<f64> {
        self.best_geo().map(|g| g.latitude)
    }

    fn best_longitude(&self) -> Option<f64> {
        self.best_geo().map(|g| g.longitude)
    }

    fn best_altitude(&self) -> Option<f64> {
        self.best_geo().map(|g| g.altitude)
    }
}

fn is_zero_gps(geo: &GeoData) -> bool {
    geo.latitude == 0.0 && geo.longitude == 0.0
}

// MARK: - Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_metadata_conversion() {
        let json = r#"{
            "title": "sunset.jpg",
            "description": "A beautiful sunset",
            "photoTakenTime": { "timestamp": "1700000000" },
            "geoDataExif": { "latitude": 22.3193, "longitude": 114.1694, "altitude": 100.0 },
            "favorited": true
        }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        let meta = takeout.to_photo_metadata();

        assert_eq!(meta.title.as_deref(), Some("sunset.jpg"));
        assert_eq!(meta.description.as_deref(), Some("A beautiful sunset"));
        assert_eq!(meta.creation_date.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(meta.latitude, Some(22.3193));
        assert_eq!(meta.longitude, Some(114.1694));
        assert_eq!(meta.altitude, Some(100.0));
        assert_eq!(meta.is_favorite, Some(true));
    }

    #[test]
    fn test_zero_timestamp() {
        let json = r#"{ "photoTakenTime": { "timestamp": "0" } }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        assert_eq!(takeout.to_photo_metadata().creation_date, None);
    }

    #[test]
    fn test_empty_timestamp() {
        let json = r#"{ "photoTakenTime": { "timestamp": "" } }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        assert_eq!(takeout.to_photo_metadata().creation_date, None);
    }

    #[test]
    fn test_negative_timestamp() {
        // 1960-01-01 00:00:00 UTC = -315619200
        let json = r#"{ "photoTakenTime": { "timestamp": "-315619200" } }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        let meta = takeout.to_photo_metadata();
        assert_eq!(meta.creation_date.as_deref(), Some("1960-01-01T00:00:00Z"));
    }

    #[test]
    fn test_zero_gps_skipped() {
        let json = r#"{
            "geoDataExif": { "latitude": 0.0, "longitude": 0.0, "altitude": 0.0 },
            "geoData": { "latitude": 22.3, "longitude": 114.2, "altitude": 50.0 }
        }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        let meta = takeout.to_photo_metadata();
        assert_eq!(meta.latitude, Some(22.3));
        assert_eq!(meta.longitude, Some(114.2));
    }

    #[test]
    fn test_both_gps_zero() {
        let json = r#"{
            "geoDataExif": { "latitude": 0.0, "longitude": 0.0, "altitude": 0.0 },
            "geoData": { "latitude": 0.0, "longitude": 0.0, "altitude": 0.0 }
        }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        let meta = takeout.to_photo_metadata();
        assert_eq!(meta.latitude, None);
        assert_eq!(meta.longitude, None);
    }

    #[test]
    fn test_absent_favorited_defaults_false() {
        let json = r#"{}"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        assert_eq!(takeout.to_photo_metadata().is_favorite, Some(false));
    }

    #[test]
    fn test_trashed() {
        let json = r#"{ "trashed": true }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        assert!(takeout.is_trashed());
    }

    #[test]
    fn test_not_trashed_when_absent() {
        let json = r#"{}"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        assert!(!takeout.is_trashed());
    }

    #[test]
    fn test_geo_data_exif_preferred_over_geo_data() {
        let json = r#"{
            "geoDataExif": { "latitude": 1.0, "longitude": 2.0, "altitude": 3.0 },
            "geoData": { "latitude": 4.0, "longitude": 5.0, "altitude": 6.0 }
        }"#;
        let takeout: TakeoutJson = serde_json::from_str(json).unwrap();
        let meta = takeout.to_photo_metadata();
        assert_eq!(meta.latitude, Some(1.0));
        assert_eq!(meta.longitude, Some(2.0));
    }
}
