import AppKit
import DarkslideCore
import DarkslideFFI
import DarkslideUI
import SwiftUI
import UniformTypeIdentifiers

/// ⌘E: export the selected photos through one or more process recipes.
struct ExportSheet: View {
    @Bindable var app: AppModel
    @Bindable var model: ExportModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            switch model.phase {
            case .editing:
                HStack(alignment: .top, spacing: 0) {
                    recipeList.frame(width: 260)
                    Divider()
                    editor.frame(maxWidth: .infinity)
                }
            case .running(let done, let total):
                running(done: done, total: total)
            case .finished(let outcome):
                results(outcome)
            case .failed(let message):
                failure(message)
            }
            Divider()
            footer
        }
        .frame(width: 720, height: 560)
        .background(DS.panelBackground)
        .onDisappear { app.endExport() }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: DS.Space.m) {
            Image(systemName: "square.and.arrow.up")
                .font(.system(size: 18))
                .foregroundStyle(DS.accent)
                .frame(width: 36, height: 36)
                .background(DS.accent.opacity(0.14), in: .rect(cornerRadius: DS.Radius.m))
            VStack(alignment: .leading, spacing: 2) {
                Text("Export \(model.items.count) \(model.items.count == 1 ? "Photo" : "Photos")")
                    .font(.system(size: 15, weight: .semibold))
                Text("\(model.enabledRecipes.count) recipe\(model.enabledRecipes.count == 1 ? "" : "s") · "
                     + "\(model.outputCount) file\(model.outputCount == 1 ? "" : "s") into \(folderLabel)")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .lineLimit(1).truncationMode(.middle)
                if let note = model.skippedVideosNote {
                    Text(note).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                }
            }
            Spacer()
        }
        .padding(DS.Space.m)
    }

    private var folderLabel: String {
        (model.defaultFolder.path as NSString).abbreviatingWithTildeInPath
    }

    // MARK: Recipes

    private var recipeList: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("RECIPES").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
                .padding([.horizontal, .top], DS.Space.m).padding(.bottom, DS.Space.xs)
            List(selection: $model.selectedRecipeID) {
                ForEach($model.recipes) { $r in
                    HStack(spacing: DS.Space.s) {
                        Toggle("", isOn: $r.enabled).labelsHidden().toggleStyle(.checkbox)
                            .accessibilityLabel("Export with \(r.name)")
                        VStack(alignment: .leading, spacing: 1) {
                            Text(r.name).font(.system(size: 12, weight: .medium)).lineLimit(1)
                            Text(r.summary).font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1)
                        }
                    }
                    .tag(r.id)
                }
            }
            .listStyle(.sidebar)
            HStack(spacing: DS.Space.xs) {
                Button { model.addRecipe() } label: { Image(systemName: "plus") }
                    .help("Add a recipe").accessibilityLabel("Add recipe")
                Button { if let id = model.selectedRecipeID { model.removeRecipe(id) } } label: { Image(systemName: "minus") }
                    .help("Remove the selected recipe").accessibilityLabel("Remove recipe")
                    .disabled(model.recipes.count < 2)
                Button("Duplicate") { model.duplicateSelected() }
                Spacer()
            }
            .buttonStyle(.borderless)
            .padding(DS.Space.s)
        }
    }

    @ViewBuilder private var editor: some View {
        if let binding = Binding($model.selectedRecipe) {
            RecipeEditor(recipe: binding, model: model, proofProfile: app.softProof.recipeProfile)
        } else {
            Text("Select a recipe").foregroundStyle(DS.textSecondary).frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    // MARK: Run

    private func running(done: Int, total: Int) -> some View {
        VStack(spacing: DS.Space.m) {
            Spacer()
            ProgressView(value: Double(model.uploading?.done ?? done), total: Double(max(model.uploading?.total ?? total, 1)))
                .frame(width: 360)
            Text(model.runningText ?? "Exporting \(done) of \(total)…").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                .monospacedDigit()
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private func results(_ o: ExportOutcome) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                Label(ExportModel.summary(o), systemImage: o.failures.isEmpty && !o.cancelled
                      ? "checkmark.circle" : "exclamationmark.triangle")
                    .font(.system(size: 13, weight: .medium))
                Spacer()
                Button("Show All in Finder") { model.showInFinder(o.written.map(\.url)) }
                    .disabled(o.written.isEmpty)
            }
            ForEach(Array(model.googleUploads.enumerated()), id: \.offset) { _, u in
                Label("\(u.recipeName): \(u.summary)", systemImage: u.failed.isEmpty && u.error == nil && !u.cancelled
                      ? "checkmark.icloud" : "exclamationmark.icloud")
                    .font(DS.Font.label)
            }
            List {
                ForEach(o.failures, id: \.self) { f in
                    Label(f, systemImage: "xmark.octagon").foregroundStyle(DS.reject).font(DS.Font.label)
                }
                ForEach(model.googleUploads.flatMap(\.failed), id: \.self) { f in
                    Label("\(f.url.lastPathComponent) not uploaded: \(f.reason)", systemImage: "xmark.icloud")
                        .foregroundStyle(DS.reject).font(DS.Font.label)
                }
                ForEach(o.written) { w in
                    HStack {
                        Text(w.url.lastPathComponent).font(.system(size: 12)).lineLimit(1).truncationMode(.middle)
                        Spacer()
                        Text("\(w.width)×\(w.height) · \(ByteCountFormatter.string(fromByteCount: w.bytes, countStyle: .file))")
                            .font(DS.Font.value).foregroundStyle(DS.textSecondary)
                        Button { model.showInFinder([w.url]) } label: { Image(systemName: "magnifyingglass") }
                            .buttonStyle(.borderless).help("Show in Finder")
                            .accessibilityLabel("Show \(w.url.lastPathComponent) in Finder")
                    }
                }
            }
        }
        .padding(DS.Space.m)
    }

    private func failure(_ message: String) -> some View {
        VStack(spacing: DS.Space.s) {
            Spacer()
            Label("Export failed", systemImage: "exclamationmark.triangle").font(.system(size: 13, weight: .medium))
            Text(message).font(DS.Font.label).foregroundStyle(DS.textSecondary).multilineTextAlignment(.center)
            Spacer()
        }
        .frame(maxWidth: .infinity)
        .padding(DS.Space.m)
    }

    // MARK: Footer

    private var footer: some View {
        HStack {
            if case .editing = model.phase, let p = model.problem {
                Label(p, systemImage: "exclamationmark.circle").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .lineLimit(2)
            }
            Spacer()
            switch model.phase {
            case .editing:
                Button("Cancel", role: .cancel) { app.endExport() }.keyboardShortcut(.cancelAction)
                Button("Export") { model.start() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent).tint(DS.accent)
                    .disabled(!model.canStart)
            case .running:
                Button("Stop", role: .cancel) { model.cancel() }.keyboardShortcut(.cancelAction)
            case .finished, .failed:
                Button("Export Again") { model.reset() }
                Button("Done") { app.endExport() }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(DS.Space.m)
    }
}

/// Editing one recipe.
private struct RecipeEditor: View {
    @Binding var recipe: ExportRecipe
    let model: ExportModel
    /// The soft proof's profile, when a recipe can export to it.
    let proofProfile: RecipeOutputProfile?

    var body: some View {
        Form {
            TextField("Name", text: $recipe.name)
            Picker("Format", selection: $recipe.format) {
                ForEach(ExportRecipe.Format.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            if recipe.format.hasQuality {
                LabeledContent("Quality") {
                    HStack {
                        Slider(value: Binding(get: { Double(recipe.quality) }, set: { recipe.quality = Int($0.rounded()) }),
                               in: 1...100)
                        Text("\(recipe.quality)").font(DS.Font.value).frame(width: 28, alignment: .trailing)
                    }
                }
            }
            Picker("Size", selection: $recipe.sizeMode) {
                ForEach(ExportRecipe.SizeMode.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            if recipe.sizeMode != .original {
                TextField("Value", value: $recipe.sizeValue, format: .number)
                Toggle("Allow upscaling", isOn: $recipe.allowUpscale)
            }
            Toggle("AI Upscale", isOn: $recipe.aiUpscale)
                .help(model.aiUpscaleHelp)
            Toggle("Constrain to sensor pixels", isOn: $recipe.constrainToSensorPixels)
                .help("Export a straightened or lens-corrected photo at its inscribed size instead of scaling it back "
                    + "up to the original size.")
            if let p = recipe.outputProfile {
                LabeledContent("ICC profile") {
                    HStack {
                        Text("\(p.name) · \(p.intent.title)\(p.blackPointCompensation ? " · BPC" : "")")
                            .lineLimit(1).truncationMode(.middle).foregroundStyle(DS.textSecondary)
                        Button("Remove") { recipe.outputProfile = nil }
                    }
                }
            } else {
                Picker("Color space", selection: $recipe.space) {
                    ForEach(ExportRecipe.Space.allCases, id: \.self) { Text($0.title).tag($0) }
                }
                if let proofProfile {
                    Button("Use Proof Profile (\(proofProfile.name))") { recipe.outputProfile = proofProfile }
                        .help("Convert to the soft proof's profile, with its intent, and embed it")
                }
            }
            LabeledContent("Sharpening") {
                HStack {
                    Slider(value: $recipe.sharpen, in: 0...2)
                    Text(recipe.sharpen == 0 ? "Off" : recipe.sharpen.formatted(.number.precision(.fractionLength(1))))
                        .font(DS.Font.value).frame(width: 28, alignment: .trailing)
                }
            }
            TextField("File name", text: $recipe.fileNameTemplate)
                .help("Tokens: {name} {seq} {seq:3} {date} {date:%Y-%m-%d} {time} {camera}")
            if let sample = model.sampleFileName(recipe) {
                LabeledContent("Example") { Text(sample).font(DS.Font.value).foregroundStyle(DS.textSecondary) }
            }
            LabeledContent("Destination") {
                HStack {
                    Text(destinationLabel).lineLimit(1).truncationMode(.middle).foregroundStyle(DS.textSecondary)
                    Button("Choose…", action: chooseFolder)
                    if recipe.destination != nil { Button("Default") { recipe.destination = nil } }
                }
            }
            if recipe.destination == nil, let note = model.destinationNote {
                Label(note, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.warningText)
            }
            TextField("Subfolder", text: $recipe.subfolder)
            Toggle("Include metadata (EXIF)", isOn: $recipe.includeMetadata)
            Toggle("Remove location (GPS, city, place)", isOn: $recipe.stripGPS).disabled(!recipe.includeMetadata)
                .help("Leave out the GPS position and the city, state, country and sublocation fields")
            Section("Google Photos") {
                Toggle("Also upload to Google Photos", isOn: Binding(
                    get: { recipe.googlePhotos != nil },
                    set: { recipe.googlePhotos = $0 ? (recipe.googlePhotos ?? GooglePhotosTarget()) : nil }))
                    .help("Upload this recipe's JPEG or HEIC files into an album in your Google Photos")
                if let binding = Binding($recipe.googlePhotos) {
                    GooglePhotosAlbumPicker(target: binding, model: model)
                }
            }
            Section("Watermark") {
                Toggle("Add a watermark", isOn: Binding(get: { recipe.watermark != nil },
                                                        set: { recipe.watermark = $0 ? ExportWatermark() : nil }))
                if let binding = Binding($recipe.watermark) {
                    WatermarkEditor(watermark: binding, preview: model.watermarkPreview(for: recipe.watermark))
                }
            }
            .task(id: recipe.watermark) { await model.updateWatermarkPreview(recipe) }
            PostProcessSection(recipe: $recipe, available: PluginStore.shared.postProcessors)
        }
        .formStyle(.grouped)
    }

    private var destinationLabel: String {
        let url = recipe.directory(defaultFolder: model.defaultFolder)
        return (url.path as NSString).abbreviatingWithTildeInPath
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        if panel.runModal() == .OK, let url = panel.url { recipe.destination = url }
    }
}

/// The album a recipe uploads into: one of this app's albums, or a new one by name.
private struct GooglePhotosAlbumPicker: View {
    @Binding var target: GooglePhotosTarget
    let model: ExportModel

    var body: some View {
        Picker("Album", selection: Binding(
            get: { target.albumID },
            set: { id in
                target.albumID = id
                if let id, let a = model.googleAlbums.first(where: { $0.id == id }) { target.albumTitle = a.title }
            })) {
            Text("New or existing by name").tag(String?.none)
            ForEach(model.googleAlbums) { a in Text(a.title).tag(Optional(a.id)) }
            if let id = target.albumID, !model.googleAlbums.contains(where: { $0.id == id }) {
                Text(target.albumTitle).tag(Optional(id))
            }
        }
        .task { await model.loadGoogleAlbums() }
        if target.albumID == nil {
            TextField("Album name", text: $target.albumTitle)
                .help("Darkslide adds to its own album with this name, or creates it")
        }
        if let m = model.googleAlbumsMessage {
            Label(m, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.warningText)
        }
        Text("The photo's caption (else its title) becomes the description in Google Photos.")
            .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
    }
}

/// A recipe's plug-in export steps (Settings ▸ Plugins adds them): a switch per step,
/// and its parameters when on. They run after the watermark, in this order.
private struct PostProcessSection: View {
    @Binding var recipe: ExportRecipe
    let available: [PostProcessorInfo]

    var body: some View {
        Section("Post-processing") {
            if available.isEmpty && recipe.postProcess.isEmpty {
                Text("Export steps from plug-ins appear here (Settings ▸ Plugins).")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            ForEach(available, id: \.stepID) { info in
                Toggle("“\(info.name)” (\(info.pluginName))", isOn: Binding(get: { recipe.runs(info) },
                                                                        set: { recipe.setPostProcess(info, on: $0) }))
                if let i = recipe.postProcess.firstIndex(where: { $0.matches(info) }) {
                    ForEach(info.params, id: \.key) { p in ParamControl(step: $recipe.postProcess[i], param: p) }
                    ForEach(PluginStore.shared.notes(for: recipe.postProcess[i]), id: \.self) { note in
                        Label(note, systemImage: "exclamationmark.triangle").font(DS.Font.caption)
                            .foregroundStyle(DS.warningText)
                    }
                    if let note = recipe.versionNote(recipe.postProcess[i], available: available) {
                        Label(note, systemImage: "exclamationmark.triangle").font(DS.Font.caption)
                            .foregroundStyle(DS.warningText)
                    }
                }
            }
            ForEach(recipe.missingPostProcess(available: available)) { step in
                HStack {
                    Label("\(step.name) isn't available (its plug-in is off or removed)", systemImage: "exclamationmark.triangle")
                        .font(DS.Font.caption).foregroundStyle(DS.warningText)
                    Button("Remove") { recipe.postProcess.removeAll { $0 == step } }
                }
            }
        }
    }
}

private struct ParamControl: View {
    @Binding var step: RecipePostProcess
    let param: PluginParamInfo

    var body: some View {
        switch param.kind {
        case "float", "int":
            LabeledContent(param.label) {
                HStack {
                    Slider(value: Binding(get: { step.number(param) }, set: { step.setNumber($0, for: param) }),
                           in: (param.min ?? 0)...(param.max ?? 1))
                    Text(step.number(param).formatted(.number.precision(.fractionLength(param.kind == "int" ? 0 : 2))))
                        .font(DS.Font.value).frame(width: 40, alignment: .trailing)
                }
            }
        case "bool":
            Toggle(param.label, isOn: Binding(get: { step.bool(param) }, set: { step.set(.bool($0), for: param) }))
        case "choice":
            Picker(param.label, selection: Binding(get: { step.text(param) }, set: { step.set(.text($0), for: param) })) {
                ForEach(param.choices, id: \.self) { Text($0).tag($0) }
            }
        case "color":
            ColorPicker(param.label, selection: Binding(get: { step.color(param) }, set: { step.setColor($0, for: param) }))
        default:
            TextField(param.label, text: Binding(get: { step.text(param) }, set: { step.set(.text($0), for: param) }))
        }
    }
}

/// A recipe's watermark: text or logo, position, size and opacity, with a live
/// preview on the focused photo.
private struct WatermarkEditor: View {
    @Binding var watermark: ExportWatermark
    let preview: WatermarkPreviewResult?

    private static let families = NSFontManager.shared.availableFontFamilies

    var body: some View {
        if let image = preview?.image {
            Image(decorative: image, scale: 1)
                .resizable().aspectRatio(contentMode: .fit)
                .frame(maxWidth: .infinity, maxHeight: 220)
                .accessibilityLabel("Watermark preview")
        }
        if let message = preview?.message {
            Label(message, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.warningText)
        }
        Picker("Kind", selection: $watermark.kind) {
            ForEach(ExportWatermark.Kind.allCases, id: \.self) { Text($0.title).tag($0) }
        }
        .pickerStyle(.segmented)
        switch watermark.kind {
        case .text:
            TextField("Text", text: $watermark.text)
                .help("Tokens: {copyright} {artist} {year} {filename}. Copyright and artist come from Settings › General "
                    + "(else the photo's own metadata).")
            Picker("Font", selection: $watermark.fontFamily) {
                Text("System").tag("")
                ForEach(Self.families, id: \.self) { Text($0).tag($0) }
            }
            percentSlider("Size", value: $watermark.textSize, range: 0.005...0.2)
            ColorPicker("Color", selection: $watermark.cgColor, supportsOpacity: false)
            Toggle("Shadow", isOn: $watermark.shadow)
        case .logo:
            LabeledContent("Logo") {
                HStack {
                    Text(watermark.logo?.lastPathComponent ?? "None").lineLimit(1).truncationMode(.middle)
                        .foregroundStyle(DS.textSecondary)
                    Button("Choose…", action: chooseLogo)
                }
            }
            .help("A PNG with transparency, in sRGB (a colour profile in the file is ignored). Darkslide keeps its own copy.")
            percentSlider("Width", value: $watermark.logoScale, range: 0.02...1)
        }
        Picker("Position", selection: $watermark.anchor) {
            ForEach(ExportWatermark.Anchor.allCases, id: \.self) { Text($0.title).tag($0) }
        }
        percentSlider("Margin", value: $watermark.inset, range: 0...0.25)
        percentSlider("Opacity", value: $watermark.opacity, range: 0.05...1)
    }

    private func percentSlider(_ title: String, value: Binding<Double>, range: ClosedRange<Double>) -> some View {
        LabeledContent(title) {
            HStack {
                Slider(value: value, in: range)
                Text(value.wrappedValue.formatted(.percent.precision(.fractionLength(0...1))))
                    .font(DS.Font.value).frame(width: 44, alignment: .trailing)
            }
        }
    }

    private func chooseLogo() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowedContentTypes = [.png]
        panel.prompt = "Choose"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        // The recipe keeps its own copy, so moving the original doesn't break it.
        if let copy = try? WatermarkLogos.importLogo(url) { watermark.logo = copy } else { NSSound.beep() }
    }
}
