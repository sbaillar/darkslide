import LuxtableFFI
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

private var fm: FileManager { .default }

private func tempDir(_ tag: String) throws -> URL {
    let d = fm.temporaryDirectory.appendingPathComponent("meta-\(tag)-\(UUID().uuidString)", isDirectory: true)
    try fm.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private func fields(_ set: (inout IPTCMetadata) -> Void) -> IPTCMetadata {
    var m = IPTCMetadata()
    set(&m)
    return m
}

// MARK: - IPTC fields and the Mixed logic

@MainActor
@Suite struct MetadataFieldTests {
    @Test func fieldsSubscriptOverlayAndSearch() {
        var m = IPTCMetadata()
        #expect(m.isEmpty && m.setFields.isEmpty)
        for (i, f) in IPTCMetadata.Field.allCases.enumerated() { m[f] = "v\(i)" }
        #expect(IPTCMetadata.Field.allCases.enumerated().allSatisfy { m[$1] == "v\($0)" }, "each field is its own storage")
        #expect(Set(IPTCMetadata.Field.allCases.map(\.label)).count == 12)
        #expect(IPTCMetadata.Field.caption.isLong && !IPTCMetadata.Field.title.isLong)
        let base = fields { $0.title = "Pier"; $0.creator = "Camera" }
        let preset = fields { $0.creator = "Alexa"; $0.copyright = "© Alexa" }
        let o = base.overlaid(preset)
        #expect(o.title == "Pier" && o.creator == "Alexa" && o.copyright == "© Alexa")
        let t = fields { $0.title = "Sunset over Sicily"; $0.caption = "Two surfers at DAWN" }
        #expect(t.matches("sicily") && t.matches("dawn") && !t.matches("akureyri"))
    }

    @Test func mixedValuesAreShownAndOnlyWrittenWhenChanged() {
        var a = samplePhoto(1), b = samplePhoto(2)
        a.metadata.title = "Pier"
        b.metadata.title = "Beach"
        a.metadata.creator = "Alexa"
        b.metadata.creator = "Alexa"
        #expect(MetadataEditing.shown(.title, in: [a, b]) == .mixed)
        #expect(MetadataEditing.shown(.creator, in: [a, b]) == .value("Alexa"))
        #expect(MetadataEditing.shown(.city, in: [a, b]) == .value(""))
        #expect(MetadataEditing.shown(.title, in: []) == .value(""))
        #expect(MetadataEditing.initialText(.mixed) == "" && MetadataEditing.initialText(.value("x")) == "x")
        // A Mixed field left empty keeps everyone's value; typing replaces it.
        #expect(MetadataEditing.change(typed: "  ", shown: .mixed) == nil)
        #expect(MetadataEditing.change(typed: " Dock ", shown: .mixed) == "Dock")
        // An unchanged field isn't written; a cleared one is.
        #expect(MetadataEditing.change(typed: "Alexa ", shown: .value("Alexa")) == nil)
        #expect(MetadataEditing.change(typed: "", shown: .value("Alexa")) == "")
    }

    @Test func modelEditsTheSelectionOneFieldAtATimeWithUndo() async {
        let m = makeModel(counts: [4])
        await m.start()
        let ids = Array(m.visibleIDs.prefix(2))
        let edited = ids.map { m.photo($0)?.isEdited }
        m.selection = Set([ids[0]])
        m.commitMetadata(.title, typed: "Pier")
        m.selection = Set([ids[1]])
        m.commitMetadata(.title, typed: "Beach")
        m.selection = Set(ids)
        #expect(MetadataEditing.shown(.title, in: m.metadataTargets) == .mixed)
        // Writing the creator for both leaves each title alone.
        m.commitMetadata(.creator, typed: "Alexa")
        #expect(m.photo(ids[0])?.metadata.title == "Pier" && m.photo(ids[1])?.metadata.title == "Beach")
        #expect(m.metadataTargets.allSatisfy { $0.metadata.creator == "Alexa" })
        // An empty commit into the Mixed title changes nothing (no undo step).
        m.commitMetadata(.title, typed: "")
        m.undo()
        #expect(m.metadataTargets.allSatisfy { $0.metadata.creator.isEmpty }, "the creator was the last step")
        #expect(m.photo(ids[0])?.metadata.title == "Pier")
        m.redo()
        #expect(m.metadataTargets.allSatisfy { $0.metadata.creator == "Alexa" })
        // Clear for All empties a Mixed field on every photo, as one undo step.
        m.clearMetadata(.title)
        #expect(m.metadataTargets.allSatisfy { $0.metadata.title.isEmpty && $0.metadata.creator == "Alexa" })
        m.undo()
        #expect(MetadataEditing.shown(.title, in: m.metadataTargets) == .mixed)
        // Editing metadata never touches the edit.
        #expect(ids.map { m.photo($0)?.isEdited } == edited)
    }

    /// Review H1: an edit commits to the photos it was opened for, not whatever is
    /// selected when the field loses focus, and a repeat commit writes nothing.
    @Test func anEditCommitsToThePhotosItWasOpenedFor() async {
        let m = makeModel(counts: [4])
        await m.start()
        let ids = Array(m.visibleIDs.prefix(2))
        m.selection = Set([ids[0]])
        m.commitMetadata(.title, typed: "Pier")
        let edit = m.beginMetadataEdit(.title)
        #expect(edit.ids == [ids[0]] && edit.baseline == .value("Pier"))
        m.selection = Set([ids[1]])
        // The box still holds A's "Pier": committing it must not touch B.
        var e = m.commitMetadata(edit, typed: "Pier")
        #expect(m.photo(ids[1])?.metadata.title == "")
        e = m.commitMetadata(e, typed: "Dock")
        #expect(m.photo(ids[0])?.metadata.title == "Dock" && m.photo(ids[1])?.metadata.title == "")
        #expect(e.baseline == .value("Dock"))
        // Return then focus loss: the second commit of the same text is no new step.
        m.commitMetadata(e, typed: "Dock")
        m.undo()
        #expect(m.photo(ids[0])?.metadata.title == "Pier")
    }

    @Test func searchFieldMatchesTitlesAndCaptions() {
        var p = samplePhoto(1)
        p.metadata.caption = "Lava flow at night"
        var f = PhotoFilter()
        f.text = "lava"
        #expect(f.matches(p))
        f.text = "glacier"
        #expect(!f.matches(p))
        f.text = p.filename
        #expect(f.matches(p))
    }
}

// MARK: - Presets

@MainActor
@Suite struct MetadataPresetTests {
    @Test func presetsSaveApplyAndPickOneForImport() async throws {
        let dir = try tempDir("presets")
        defer { try? fm.removeItem(at: dir) }
        let url = dir.appendingPathComponent("metadata-presets.json")
        let store = MetadataPresetStore(fileURL: url)
        #expect(store.save(name: " ", fields: fields { $0.creator = "x" }) == nil && store.lastError != nil)
        #expect(store.save(name: "Empty", fields: IPTCMetadata()) == nil)
        let credits = fields { $0.creator = "Alexa"; $0.copyright = "© 2026 Alexa" }
        // Review L8: a preset keeps only credits and rights, and a success clears the error.
        let photo = fields { $0.title = "Pier"; $0.creator = "Alexa"; $0.copyright = "© 2026 Alexa"; $0.city = "Akureyri" }
        #expect(photo.credits == credits)
        let a = try #require(store.save(name: "Credits", fields: credits))
        let b = try #require(store.save(name: "Credits", fields: fields { $0.city = "Akureyri" }))
        #expect(b.name == "Credits 2", "names stay unique")
        #expect(store.lastError == nil, "the earlier name error is gone after a save")
        store.setImportPreset(a.id)
        #expect(store.importPreset?.id == a.id)
        store.setImportPreset(b.id)
        #expect(store.importPreset?.id == b.id && store.presets.filter(\.applyOnImport).count == 1)
        // Persisted, and read back by a new store.
        let again = MetadataPresetStore(fileURL: url)
        #expect(again.presets.map(\.name) == ["Credits", "Credits 2"] && again.importPreset?.id == b.id)
        again.delete(b.id)
        #expect(MetadataPresetStore(fileURL: url).presets.map(\.name) == ["Credits"])
        store.setImportPreset(nil)
        #expect(store.importPreset == nil)

        // Applying sets only the preset's fields, on the selection, as one undo step.
        let m = makeModel(counts: [3])
        await m.start()
        let ids = Array(m.visibleIDs.prefix(2))
        m.selection = Set([ids[0]])
        m.commitMetadata(.title, typed: "Pier")
        m.selection = Set(ids)
        m.applyMetadataPreset(a)
        #expect(m.metadataTargets.allSatisfy { $0.metadata.creator == "Alexa" && $0.metadata.copyright == "© 2026 Alexa" })
        #expect(m.photo(ids[0])?.metadata.title == "Pier")
        m.undo()
        #expect(m.metadataTargets.allSatisfy { $0.metadata.creator.isEmpty })
    }

    @Test func aDamagedFileIsBackedUpNotOverwritten() throws {
        let dir = try tempDir("presets-bad")
        defer { try? fm.removeItem(at: dir) }
        let url = dir.appendingPathComponent("metadata-presets.json")
        try Data("not json".utf8).write(to: url)
        let store = MetadataPresetStore(fileURL: url)
        #expect(store.presets.isEmpty && store.lastError?.contains("backup") == true)
        #expect(fm.fileExists(atPath: url.appendingPathExtension("bak").path))
        #expect(MetadataPresetStore(fileURL: nil).presets.isEmpty)
    }
}

// MARK: - Sidecars, import and export (FFI)

@MainActor
@Suite struct MetadataStorageTests {
    @Test func recordMappingAndWritePlan() {
        let m = fields {
            $0.title = "t"; $0.caption = "c"; $0.creator = "cr"; $0.copyright = "©"; $0.credit = "cl"; $0.headline = "h"
            $0.city = "ci"; $0.state = "st"; $0.country = "co"; $0.sublocation = "sl"; $0.instructions = "in"
            $0.usageTerms = "ut"
        }
        #expect(RustSessionStore.metadata(RustSessionStore.record(m)) == m, "lossless both ways")
        var a = samplePhoto(1)
        let stored = a
        a.metadata.title = "x"
        let plan = RustSessionStore.writePlan([a], stored: [a.id: stored])
        #expect(plan.metadata == [a.metadata: [a.id.raw]] && plan.keywords.isEmpty && plan.ratings.isEmpty)
        #expect(RustSessionStore.writePlan([a], stored: [a.id: a]).metadata.isEmpty)
        var r = VariantRecord(variantId: 1, imageId: 1, originalPath: "/x/A.ARW", fileName: "A.ARW", name: "", rating: 0,
                              colorTag: .none, pick: .unflagged, edited: false, readOnly: false, missing: false,
                              fileChanged: false, keptInStore: false, fileSize: 1, fileSequence: nil, captureTimeMs: nil,
                              width: 1, height: 1, meta: nil)
        #expect(RustSessionStore.photo(from: r).metadata.isEmpty)
        r.metadata = RustSessionStore.record(m)
        #expect(RustSessionStore.photo(from: r).metadata == m)
    }

    @Test func importOptionsCarryTheImportPreset() async throws {
        let lib = try tempDir("imp-lib")
        defer { try? fm.removeItem(at: lib) }
        let preset = fields { $0.creator = "Alexa" }
        let plan = ImportPlan(candidates: [], destination: .newSession(name: "Trip"), renameTemplate: "", backupFolder: nil,
                              volume: URL(fileURLWithPath: "/Volumes/METACARD"))
        let with = RustImportService(libraryRoot: lib, globalIndex: nil, metadataPreset: { preset })
        #expect(try await with.options(for: plan).metadataPreset?.creator == "Alexa")
        let none = RustImportService(libraryRoot: lib, globalIndex: nil, metadataPreset: { IPTCMetadata() })
        #expect(try await none.options(for: plan).metadataPreset == nil)
    }

    /// ImageIO (HEIC and the hardware JPEG path) writes the fields as IPTC.
    @Test func imageIOEncoderWritesTheFields() throws {
        let m = fields {
            $0.title = "Pier"; $0.caption = "Night swim"; $0.creator = "Alexa"; $0.copyright = "© Alexa"
            $0.city = "Akureyri"; $0.usageTerms = "Editorial"
        }
        let dict = try #require(ImageIOEncoder.iptcDictionary(keywords: ["k"], metadata: RustSessionStore.record(m)))
        #expect(dict[kCGImagePropertyIPTCByline] as? [String] == ["Alexa"])
        #expect(ImageIOEncoder.iptcDictionary(keywords: [], metadata: RustSessionStore.record(IPTCMetadata())) == nil)
        let e = ImageIOEncoder()
        for format in [EncodedImageFormat.jpeg, .heic] {
            let data = try e.encode(format: format, width: 64, height: 64, rgb16: Data(count: 64 * 64 * 6), quality: 80,
                                    icc: Data(), exif: nil, keywords: ["beach"], metadata: RustSessionStore.record(m))
            let src = try #require(CGImageSourceCreateWithData(data as CFData, nil))
            let iptc = (CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any])?[kCGImagePropertyIPTCDictionary]
                as? [CFString: Any]
            #expect(iptc?[kCGImagePropertyIPTCObjectName] as? String == "Pier", "\(format)")
            #expect(iptc?[kCGImagePropertyIPTCCaptionAbstract] as? String == "Night swim")
            #expect(iptc?[kCGImagePropertyIPTCCity] as? String == "Akureyri")
            #expect(iptc?[kCGImagePropertyIPTCKeywords] as? [String] == ["beach"])
            #expect(iptc?[kCGImagePropertyIPTCByline] as? [String] == ["Alexa"])
            #expect(iptc?[kCGImagePropertyIPTCCopyrightNotice] as? String == "© Alexa")
        }
    }

    /// Live: the fields persist in the sidecars and reach an exported JPEG (mozjpeg path:
    /// XMP, IPTC-IIM, and the caption as EXIF ImageDescription).
    @Test func fieldsPersistAndAreExported() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let base = try tempDir("store")
        defer { try? fm.removeItem(at: base) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await store.add(folder: dir)
        var photo = try #require(try await store.photos(in: info).first)
        photo.metadata = fields { $0.title = "Morning Surf"; $0.caption = "Dawn patrol"; $0.creator = "Kai"; $0.city = "Akureyri" }
        try await store.save(photos: [photo])
        let reopened = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info2 = try await reopened.add(folder: dir)
        #expect(try await reopened.photos(in: info2).first?.metadata == photo.metadata)

        let out = try tempDir("out")
        defer { try? fm.removeItem(at: out) }
        let url = try #require(photo.url)
        let job = ExportJob(items: [ExportJobItem(photoID: photo.id, url: url, seq: 1, metadata: photo.metadata)],
                            recipes: [ExportRecipe(name: "Small", format: .jpeg, quality: 80, sizeMode: .longEdge, sizeValue: 400)],
                            defaultFolder: out, artist: "Settings Artist", copyright: nil)
        let o = try await RustExportService().export(job) { (_: ExportProgressUpdate) in }
        let file = try #require(o.written.first?.url)
        let src = try #require(CGImageSourceCreateWithURL(file as CFURL, nil))
        let props = try #require(CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any])
        let tiff = props[kCGImagePropertyTIFFDictionary] as? [CFString: Any]
        #expect(tiff?[kCGImagePropertyTIFFImageDescription] as? String == "Dawn patrol")
        #expect(tiff?[kCGImagePropertyTIFFArtist] as? String == "Kai", "the photo's creator wins over Settings")
        let iptc = props[kCGImagePropertyIPTCDictionary] as? [CFString: Any]
        #expect(iptc?[kCGImagePropertyIPTCObjectName] as? String == "Morning Surf")
        #expect(iptc?[kCGImagePropertyIPTCCity] as? String == "Akureyri")
    }
}
