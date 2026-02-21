import Foundation
import Photos
import CoreLocation
import SwiftRs

// MARK: - Data Types

struct PhotoMetadata: Codable {
    let creationDate: String?      // ISO 8601
    let latitude: Double?
    let longitude: Double?
    let altitude: Double?
    let title: String?
    let description: String?
    let isFavorite: Bool?
}

struct ImportResult: Codable {
    let success: Bool
    let localIdentifier: String?
    let error: String?
}

struct AccessResult: Codable {
    let authorized: Bool
    let status: String
}

// MARK: - Helpers

private func authStatusString(_ status: PHAuthorizationStatus) -> String {
    switch status {
    case .notDetermined: return "not_determined"
    case .restricted: return "restricted"
    case .denied: return "denied"
    case .authorized: return "authorized"
    case .limited: return "limited"
    @unknown default: return "unknown"
    }
}

private func toJSON<T: Encodable>(_ value: T) -> String {
    let encoder = JSONEncoder()
    guard let data = try? encoder.encode(value),
          let str = String(data: data, encoding: .utf8) else {
        return "{\"error\":\"json_encoding_failed\"}"
    }
    return str
}

// MARK: - Check Access

@_cdecl("photoferry_check_access")
public func checkAccess() -> SRString {
    let semaphore = DispatchSemaphore(value: 0)
    var result = AccessResult(authorized: false, status: "unknown")

    let currentStatus = PHPhotoLibrary.authorizationStatus(for: .readWrite)

    switch currentStatus {
    case .authorized, .limited:
        result = AccessResult(
            authorized: true,
            status: authStatusString(currentStatus)
        )
    case .notDetermined:
        PHPhotoLibrary.requestAuthorization(for: .readWrite) { newStatus in
            result = AccessResult(
                authorized: newStatus == .authorized || newStatus == .limited,
                status: authStatusString(newStatus)
            )
            semaphore.signal()
        }
        semaphore.wait()
    default:
        result = AccessResult(
            authorized: false,
            status: authStatusString(currentStatus)
        )
    }

    return SRString(toJSON(result))
}

// MARK: - Import Photo

@_cdecl("photoferry_import_photo")
public func importPhoto(path: SRString, metadataJSON: SRString) -> SRString {
    let filePath = path.toString()
    let fileURL = URL(fileURLWithPath: filePath)

    guard FileManager.default.fileExists(atPath: filePath) else {
        let result = ImportResult(
            success: false,
            localIdentifier: nil,
            error: "File not found: \(filePath)"
        )
        return SRString(toJSON(result))
    }

    // Parse metadata
    var metadata: PhotoMetadata? = nil
    let metaStr = metadataJSON.toString()
    if !metaStr.isEmpty, let data = metaStr.data(using: .utf8) {
        metadata = try? JSONDecoder().decode(PhotoMetadata.self, from: data)
    }

    // Determine media type
    let ext = fileURL.pathExtension.lowercased()
    let videoExtensions: Set<String> = ["mp4", "mov", "avi", "m4v", "3gp", "mkv"]
    let isVideo = videoExtensions.contains(ext)

    let semaphore = DispatchSemaphore(value: 0)
    var localIdentifier: String? = nil
    var importError: String? = nil

    PHPhotoLibrary.shared().performChanges({
        let creationRequest: PHAssetChangeRequest

        if isVideo {
            guard let req = PHAssetChangeRequest.creationRequestForAssetFromVideo(atFileURL: fileURL) else {
                importError = "Failed to create video asset request for \(filePath)"
                return
            }
            creationRequest = req
        } else {
            guard let req = PHAssetChangeRequest.creationRequestForAssetFromImage(atFileURL: fileURL) else {
                importError = "Failed to create image asset request for \(filePath)"
                return
            }
            creationRequest = req
        }

        // Apply metadata
        if let meta = metadata {
            if let dateStr = meta.creationDate {
                let formatter = ISO8601DateFormatter()
                formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
                if let date = formatter.date(from: dateStr) {
                    creationRequest.creationDate = date
                } else {
                    formatter.formatOptions = [.withInternetDateTime]
                    if let date = formatter.date(from: dateStr) {
                        creationRequest.creationDate = date
                    }
                }
            }

            if let lat = meta.latitude, let lon = meta.longitude,
               !(lat == 0.0 && lon == 0.0) {
                if let alt = meta.altitude {
                    creationRequest.location = CLLocation(
                        coordinate: CLLocationCoordinate2D(latitude: lat, longitude: lon),
                        altitude: alt,
                        horizontalAccuracy: 0,
                        verticalAccuracy: 0,
                        timestamp: Date()
                    )
                } else {
                    creationRequest.location = CLLocation(latitude: lat, longitude: lon)
                }
            }

            if let favorite = meta.isFavorite {
                creationRequest.isFavorite = favorite
            }
        }

        localIdentifier = creationRequest.placeholderForCreatedAsset?.localIdentifier
    }) { success, error in
        if !success {
            importError = error?.localizedDescription ?? "Unknown PhotoKit error"
        }
        semaphore.signal()
    }

    semaphore.wait()

    if let err = importError {
        let result = ImportResult(success: false, localIdentifier: nil, error: err)
        return SRString(toJSON(result))
    }

    let result = ImportResult(success: true, localIdentifier: localIdentifier, error: nil)
    return SRString(toJSON(result))
}

@_cdecl("photoferry_import_live_photo")
public func importLivePhoto(photoPath: SRString, videoPath: SRString, metadataJSON: SRString) -> SRString {
    let photoFilePath = photoPath.toString()
    let videoFilePath = videoPath.toString()
    let photoURL = URL(fileURLWithPath: photoFilePath)
    let videoURL = URL(fileURLWithPath: videoFilePath)

    guard FileManager.default.fileExists(atPath: photoFilePath) else {
        let result = ImportResult(
            success: false,
            localIdentifier: nil,
            error: "File not found: \(photoFilePath)"
        )
        return SRString(toJSON(result))
    }

    guard FileManager.default.fileExists(atPath: videoFilePath) else {
        let result = ImportResult(
            success: false,
            localIdentifier: nil,
            error: "File not found: \(videoFilePath)"
        )
        return SRString(toJSON(result))
    }

    // Parse metadata
    var metadata: PhotoMetadata? = nil
    let metaStr = metadataJSON.toString()
    if !metaStr.isEmpty, let data = metaStr.data(using: .utf8) {
        metadata = try? JSONDecoder().decode(PhotoMetadata.self, from: data)
    }

    let semaphore = DispatchSemaphore(value: 0)
    var localIdentifier: String? = nil
    var importError: String? = nil

    PHPhotoLibrary.shared().performChanges({
        let req = PHAssetCreationRequest.forAsset()
        req.addResource(with: .photo, fileURL: photoURL, options: nil)
        req.addResource(with: .pairedVideo, fileURL: videoURL, options: nil)

        // Apply metadata
        if let meta = metadata {
            if let dateStr = meta.creationDate {
                let formatter = ISO8601DateFormatter()
                formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
                if let date = formatter.date(from: dateStr) {
                    req.creationDate = date
                } else {
                    formatter.formatOptions = [.withInternetDateTime]
                    if let date = formatter.date(from: dateStr) {
                        req.creationDate = date
                    }
                }
            }

            if let lat = meta.latitude, let lon = meta.longitude,
               !(lat == 0.0 && lon == 0.0) {
                if let alt = meta.altitude {
                    req.location = CLLocation(
                        coordinate: CLLocationCoordinate2D(latitude: lat, longitude: lon),
                        altitude: alt,
                        horizontalAccuracy: 0,
                        verticalAccuracy: 0,
                        timestamp: Date()
                    )
                } else {
                    req.location = CLLocation(latitude: lat, longitude: lon)
                }
            }

            if let favorite = meta.isFavorite {
                req.isFavorite = favorite
            }
        }

        localIdentifier = req.placeholderForCreatedAsset?.localIdentifier
    }) { success, error in
        if !success {
            importError = error?.localizedDescription ?? "Unknown PhotoKit error"
        }
        semaphore.signal()
    }

    semaphore.wait()

    if let err = importError {
        let result = ImportResult(success: false, localIdentifier: nil, error: err)
        return SRString(toJSON(result))
    }

    let result = ImportResult(success: true, localIdentifier: localIdentifier, error: nil)
    return SRString(toJSON(result))
}

// MARK: - Create Album

@_cdecl("photoferry_create_album")
public func createAlbum(title: SRString) -> SRString {
    let albumTitle = title.toString()
    let semaphore = DispatchSemaphore(value: 0)
    var albumIdentifier: String? = nil
    var albumError: String? = nil

    PHPhotoLibrary.shared().performChanges({
        let request = PHAssetCollectionChangeRequest.creationRequestForAssetCollection(withTitle: albumTitle)
        albumIdentifier = request.placeholderForCreatedAssetCollection.localIdentifier
    }) { success, error in
        if !success {
            albumError = error?.localizedDescription ?? "Unknown error creating album"
        }
        semaphore.signal()
    }

    semaphore.wait()

    if let err = albumError {
        return SRString("{\"error\":\"\(err)\"}")
    }
    return SRString("{\"album_id\":\"\(albumIdentifier ?? "")\"}")
}

// MARK: - Verify Assets

struct AssetVerifyResult: Codable {
    let localIdentifier: String
    let found: Bool
    let creationDate: String?
    let hasPairedVideo: Bool
}

@_cdecl("photoferry_verify_assets")
public func verifyAssets(identifiersJSON: SRString) -> SRString {
    let json = identifiersJSON.toString()
    guard let data = json.data(using: .utf8),
          let identifiers = try? JSONDecoder().decode([String].self, from: data)
    else {
        return SRString("{\"error\":\"invalid_input\"}")
    }

    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime]

    let fetchResult = PHAsset.fetchAssets(withLocalIdentifiers: identifiers, options: nil)

    var results: [AssetVerifyResult] = []
    var foundIds = Set<String>()

    fetchResult.enumerateObjects { asset, _, _ in
        foundIds.insert(asset.localIdentifier)
        let resources = PHAssetResource.assetResources(for: asset)
        let hasPaired = resources.contains {
            $0.type == .pairedVideo || $0.type == .fullSizePairedVideo
        }
        let dateStr = asset.creationDate.map { formatter.string(from: $0) }
        results.append(AssetVerifyResult(
            localIdentifier: asset.localIdentifier,
            found: true,
            creationDate: dateStr,
            hasPairedVideo: hasPaired
        ))
    }

    // Report missing assets (silently omitted by PHFetchResult)
    for id in identifiers where !foundIds.contains(id) {
        results.append(AssetVerifyResult(
            localIdentifier: id,
            found: false,
            creationDate: nil,
            hasPairedVideo: false
        ))
    }

    return SRString(toJSON(results))
}

// MARK: - Add to Album

@_cdecl("photoferry_add_to_album")
public func addToAlbum(albumID: SRString, assetID: SRString) -> Bool {
    let albumIdStr = albumID.toString()
    let assetIdStr = assetID.toString()

    let albums = PHAssetCollection.fetchAssetCollections(
        withLocalIdentifiers: [albumIdStr], options: nil
    )
    guard let album = albums.firstObject else { return false }

    let assets = PHAsset.fetchAssets(
        withLocalIdentifiers: [assetIdStr], options: nil
    )
    guard let asset = assets.firstObject else { return false }

    let semaphore = DispatchSemaphore(value: 0)
    var success = false

    PHPhotoLibrary.shared().performChanges({
        guard let request = PHAssetCollectionChangeRequest(for: album) else { return }
        request.addAssets([asset] as NSArray)
    }) { result, _ in
        success = result
        semaphore.signal()
    }

    semaphore.wait()
    return success
}
