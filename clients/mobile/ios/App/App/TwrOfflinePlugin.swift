import Foundation
import Security
import CryptoKit
import UIKit
import Capacitor

/// Only this device's Keychain holds the key; cache files are excluded from backup.
@objc(TwrOfflinePlugin)
final class TwrOfflinePlugin: CAPPlugin, CAPBridgedPlugin {
    let identifier = "TwrOfflinePlugin"
    let jsName = "TwrOffline"
    let pluginMethods: [CAPPluginMethod] = ["read", "write", "remove", "textScale"].map { CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise) }
    private let queue = DispatchQueue(label: "tools.twr.offline", qos: .utility)
    private var textObserver: NSObjectProtocol?
    private let limit = 8 * 1024 * 1024
    override func load() {
        textObserver = NotificationCenter.default.addObserver(forName: UIContentSizeCategory.didChangeNotification, object: nil, queue: .main) { [weak self] notification in
            let category = (notification.userInfo?[UIContentSizeCategory.newValueUserInfoKey] as? String).map(UIContentSizeCategory.init(rawValue:)) ?? UIApplication.shared.preferredContentSizeCategory
            let traits = UITraitCollection(preferredContentSizeCategory: category)
            self?.notifyListeners("textScale", data: ["scale": Double(UIFontMetrics(forTextStyle: .body).scaledValue(for: 17, compatibleWith: traits) / 17)])
        }
    }
    deinit { if let textObserver { NotificationCenter.default.removeObserver(textObserver) } }
    private struct Failure: LocalizedError { let message: String; var errorDescription: String? { message } }
    private func location(_ call: CAPPluginCall) throws -> (URL, String) {
        guard let id = call.getString("key"), !id.isEmpty, id.utf8.count <= 512, !id.unicodeScalars.contains(where: { $0.value < 32 }) else { throw Failure(message: "Invalid storage key") }
        var directory = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true).appendingPathComponent("twr-offline", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication, .posixPermissions: 0o700])
        var values = URLResourceValues(); values.isExcludedFromBackup = true
        try directory.setResourceValues(values)
        let name = SHA256.hash(data: Data(id.utf8)).map { String(format: "%02x", $0) }.joined()
        return (directory.appendingPathComponent(name + ".enc"), id)
    }
    private func key() throws -> SymmetricKey {
        let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: "tools.twr.offline", kSecAttrAccount as String: "aes-key"]
        var read = query; read[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(read as CFDictionary, &result)
        if status == errSecSuccess, let bytes = result as? Data { return SymmetricKey(data: bytes) }
        guard status == errSecItemNotFound else { throw Failure(message: "Unlock your device to read offline history") }
        var bytes = Data(count: 32)
        let randomStatus = bytes.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        guard randomStatus == errSecSuccess else { throw Failure(message: "Could not create offline encryption key") }
        var add = query; add[kSecValueData as String] = bytes; add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        guard SecItemAdd(add as CFDictionary, nil) == errSecSuccess else { throw Failure(message: "Could not save offline encryption key") }
        return SymmetricKey(data: bytes)
    }
    @objc func read(_ call: CAPPluginCall) {
        queue.async {
            do {
                let (url, id) = try self.location(call)
                guard FileManager.default.fileExists(atPath: url.path) else { call.resolve(["value": NSNull()]); return }
                let info = try url.resourceValues(forKeys: [.fileSizeKey, .isSymbolicLinkKey])
                guard info.isSymbolicLink != true, (info.fileSize ?? self.limit * 4) <= self.limit * 3 else { throw Failure(message: "Invalid offline history file") }
                let clear = try AES.GCM.open(AES.GCM.SealedBox(combined: Data(contentsOf: url)), using: self.key(), authenticating: Data(id.utf8))
                guard let value = String(data: clear, encoding: .utf8) else { throw Failure(message: "Invalid offline history") }
                call.resolve(["value": value])
            } catch { call.reject(error.localizedDescription) }
        }
    }
    @objc func write(_ call: CAPPluginCall) {
        queue.async {
            do {
                let (url, id) = try self.location(call)
                guard let value = call.getString("value"), value.utf8.count <= self.limit else { throw Failure(message: "Offline state exceeds 8 MiB") }
                let sealed = try AES.GCM.seal(Data(value.utf8), using: self.key(), authenticating: Data(id.utf8))
                guard let bytes = sealed.combined else { throw Failure(message: "Could not encrypt offline history") }
                let directory = url.deletingLastPathComponent()
                let files = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.fileSizeKey])
                let total = try files.filter { $0 != url }.reduce(bytes.count) { try $0 + ($1.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0) }
                guard total <= 64 * 1024 * 1024 else { throw Failure(message: "Offline storage is full. Clear saved history in Settings.") }
                try bytes.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
                call.resolve()
            } catch { call.reject(error.localizedDescription) }
        }
    }
    @objc func remove(_ call: CAPPluginCall) {
        queue.async {
            do { let (url, _) = try self.location(call); if FileManager.default.fileExists(atPath: url.path) { try FileManager.default.removeItem(at: url) }; call.resolve() }
            catch { call.reject(error.localizedDescription) }
        }
    }
    @objc func textScale(_ call: CAPPluginCall) {
        DispatchQueue.main.async { let traits = UITraitCollection(preferredContentSizeCategory: UIApplication.shared.preferredContentSizeCategory)
            call.resolve(["scale": Double(UIFontMetrics(forTextStyle: .body).scaledValue(for: 17, compatibleWith: traits) / 17)]) }
    }
}
