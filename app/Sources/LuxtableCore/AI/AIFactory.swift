import LuxtableAI
import Foundation

/// The AI service the app uses: lt-ai + Vision culling; natural-language edits
/// through `GuardedEditService`: lt-ai's rule parser first, then (for what it
/// didn't understand) Jev when enabled in Settings, then Foundation Models, with
/// the over-edit guard on every model answer.
@MainActor
func makeAIService(defaults: UserDefaults = AppModel.appDefaults) -> any AIService {
    VisionAIService(language: makeEditService(jev: JevSettings(defaults: defaults)))
}

/// The guarded natural-language pipeline (rules → Jev → Foundation Models).
func makeEditService(jev: JevSettings, foundationModels: Bool = FoundationModelsEditService.isAvailable)
    -> GuardedEditService {
    var models: [any NaturalLanguageEditService] = [JevEditService(apiKey: { jev.activeKey() })]
    if foundationModels { models.append(FoundationModelsEditService()) }
    return GuardedEditService(models: models, onModelError: { error in
        if JevSettings.isAuthFailure(error) { jev.keyRejected = true }
    })
}
