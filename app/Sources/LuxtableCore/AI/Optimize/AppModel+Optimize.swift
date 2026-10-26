import CoreGraphics
import LuxtableAI
import Foundation

extension AppModel {
    /// The photos AI Optimize works on: the selection when several are selected and the
    /// focused photo is one of them, else the focused photo (never videos).
    public var optimizeTargets: [PhotoID] {
        guard let f = focusedID else { return [] }
        let ids = selectedIDs.count > 1 && selectedIDs.contains(f) ? selectedIDs : [f]
        return ids.filter { photo($0).map { !$0.isVideo } ?? false }
    }

    public var canOptimize: Bool { optimizer != nil && optimizePanel == nil && !optimizeTargets.isEmpty }

    /// Open the AI Optimize panel and analyze the targets.
    public func beginOptimize() {
        guard let optimizer else {
            show(Toast("AI Optimize needs RAW files (it isn't available in the demo)", symbol: "sparkles", isError: true))
            return
        }
        let ids = optimizeTargets
        guard optimizePanel == nil, !ids.isEmpty else { return }
        let panel = OptimizePanelModel(app: self, ids: ids, optimizer: optimizer)
        optimizePanel = panel
        panel.start()
    }

    /// Close the panel without applying (its preview is dropped).
    public func endOptimize() {
        optimizePanel?.cancel()
        optimizePanel = nil
    }

    /// Apply AI Optimize's results as one undo step (only the values it changed, on top
    /// of anything edited since), keep each photo's report for "Why these settings?",
    /// and close the panel.
    func commitOptimize(_ results: [PhotoID: (edit: EditState, report: OptimizeReport)], bases: [PhotoID: EditState]) {
        optimizePanel = nil
        guard !results.isEmpty else { return }
        let ids = results.keys.sorted { $0.raw < $1.raw }
        let label = ids.count > 1 ? "AI Optimize \(ids.count) Photos" : "AI Optimize"
        // The reports are part of the step: undo restores the ones before (and their
        // sidecars), so a later run doesn't take the optimizer's undone values for the
        // user's resets.
        updateEdits(ids, label: label, reports: Dictionary(uniqueKeysWithValues: ids.map { id in
            (id, ReportChange(before: optimizeReports[id], after: results[id]?.report))
        }), ai: .optimize) { id, e in
            if let r = results[id], let base = bases[id] { e = AIEditMerge.apply(result: r.edit, base: base, current: e) }
        }
        endEditGesture()
        let changes = results.values.reduce(0) { $0 + $1.report.appliedChanges.count }
        show(Toast(ids.count > 1
                   ? "AI Optimize: \(changes) change\(changes == 1 ? "" : "s") on \(ids.count) photos"
                   : "AI Optimize: \(changes) change\(changes == 1 ? "" : "s") applied", symbol: "sparkles"))
    }

    /// Set (or with `nil` clear) photos' reports in memory and, in order, in their
    /// sidecars.
    func applyReports(_ reports: [PhotoID: OptimizeReport?]) {
        guard !reports.isEmpty else { return }
        for (id, r) in reports { optimizeReports[id] = r }
        guard let store = sessions as? any OptimizeReportStoring else { return }
        let previous = reportSaves
        reportSaves = Task { [weak self] in
            await previous?.value
            for (id, r) in reports {
                do { try await store.saveOptimizeReport(r, for: [id]) } catch { self?.show(error) }
            }
        }
    }

    /// The photo's last applied report: this run's, else its sidecar's. Also reads its
    /// stored AI changes the first time.
    public func loadOptimizeReport(for id: PhotoID) async -> OptimizeReport? {
        await loadAIChanges(for: id) // the stored AI changes win over the report's rows
        if let r = optimizeReports[id] { return r }
        await reportSaves?.value // an undo's clearing write lands first
        guard let store = sessions as? any OptimizeReportStoring,
              let r = try? await store.optimizeReport(for: id) else { return nil }
        optimizeReports[id] = r
        deriveAIMarks(id, from: r)
        return r
    }

    /// The report "Why these settings?" shows, if it is open.
    public var whyReport: OptimizeReport? { whyReportPhoto.flatMap { optimizeReports[$0] } }

    /// The applied changes of the open "Why" report that the photo's edit still has (an
    /// undo, a paste or a slider may have changed some since; Rust compares the values).
    public var whyChanges: [OptimizeChange] {
        guard let id = whyReportPhoto, let r = optimizeReports[id] else { return [] }
        let still = (try? optimizer?.stillApplied(r, in: edit(for: id))) ?? Set(r.appliedChanges.map(\.id))
        return r.appliedChanges.filter { still.contains($0.id) }
    }

    /// Show why the focused photo has the values AI Optimize set.
    public func showWhy() {
        guard let f = focusedID, optimizeReports[f] != nil else { return }
        whyReportPhoto = f
    }

    // MARK: - Live wiring

    /// Vision on the photo's preview (faces, horizon, saliency, labels in one pass) plus
    /// the sky mask when the sky model is installed (never downloaded for this).
    public func visionOptimizeSignals(_ photo: Photo, aiDenoiseAvailable: Bool, skyModelReady: Bool) async -> OptimizeSignals {
        guard let image = await analysisImage(for: photo) else {
            return OptimizeSignals.from(nil, sky: nil, aiDenoiseAvailable: aiDenoiseAvailable)
        }
        let analysis = try? await VisionService().analyze(image, includeLabels: true)
        let sky = skyModelReady ? await viewer.masksForRender(photo: photo.id, targets: [.sky])[.sky] : nil
        return OptimizeSignals.from(analysis, sky: sky, aiDenoiseAvailable: aiDenoiseAvailable)
    }
}

/// Texts of "Why these settings?".
public enum OptimizeWhy {
    public static let buttonLabel = "Why these settings?"
    public static let buttonHelp = "Why these settings? What AI Optimize changed on this photo, and why"
    public static let caveat = "Only values the photo still has are listed."

    /// The sentence above the explanations: `inEffect` of the report's applied changes
    /// are still the photo's values.
    public static func intro(_ r: OptimizeReport, inEffect: Int) -> String {
        let n = r.appliedChanges.count
        guard n > 0 else { return "AI Optimize looked at this photo and left its settings as they were." }
        guard inEffect > 0 else {
            return "The values AI Optimize set have all been changed since (undone, pasted over or adjusted)."
        }
        let these = inEffect == 1 ? "this value" : "these \(inEffect) values"
        let since = inEffect < n ? " (\(n - inEffect) more were changed since)" : ""
        return "AI Optimize set \(these)\(since), each chosen by a rule from measurements of this photo."
    }
}
