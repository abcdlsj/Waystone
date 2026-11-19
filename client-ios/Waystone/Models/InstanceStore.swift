import Foundation

/// The persisted pool of instances.
///
/// A single JSON file next to the keychain entries keeps the app dependency free
/// and makes the state trivially inspectable while developing.
@MainActor
final class InstanceStore: ObservableObject {
    @Published private(set) var instances: [Instance] = []

    private let fileURL: URL

    init(fileURL: URL? = nil) {
        if let fileURL {
            self.fileURL = fileURL
        } else {
            let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            let directory = base.appendingPathComponent("Waystone", isDirectory: true)
            try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            self.fileURL = directory.appendingPathComponent("instances.json")
        }
        load()
    }

    func load() {
        guard let data = try? Data(contentsOf: fileURL),
            let decoded = try? JSONDecoder().decode([Instance].self, from: data)
        else {
            instances = []
            return
        }
        instances = decoded
    }

    func save() {
        guard let data = try? JSONEncoder().encode(instances) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// Adds or replaces an instance, matching on the server id or endpoint.
    func upsert(_ instance: Instance) {
        if let index = instances.firstIndex(where: { $0.id == instance.id || $0.endpoint == instance.endpoint }) {
            instances[index] = instance
        } else {
            instances.append(instance)
        }
        save()
    }

    func remove(_ instance: Instance) {
        instances.removeAll { $0.id == instance.id }
        save()
    }

    func markSeen(_ instance: Instance) {
        guard let index = instances.firstIndex(where: { $0.id == instance.id }) else { return }
        instances[index].lastSeen = Date()
        save()
    }
}
