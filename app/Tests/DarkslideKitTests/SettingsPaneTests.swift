@testable import DarkslideMac
import DarkslideCore
import Testing

/// Settings is a sidebar: every pane stays reachable however many integrations are on
/// (toolbar tabs overflowed, and macOS greys out the ones in the overflow menu).
struct SettingsPaneTests {
    @Test func everyPaneShowsWhenEveryIntegrationIsOn() {
        let all = SettingsPane.visible(isOn: { _ in true }, googlePhotos: true, immich: true)
        #expect(all == SettingsPane.allCases)
        #expect(all.suffix(2) == [.plugins, .updates], "Plugins and Updates last, always listed")
    }

    @Test func anIntegrationThatIsOffOrUnavailableHasNoPane() {
        let none = SettingsPane.visible(isOn: { _ in false }, googlePhotos: true, immich: true)
        #expect(none == [.general, .familiar, .shortcuts, .raw, .ai, .voice, .devices, .plugins, .updates])
        let onlyImmich = SettingsPane.visible(isOn: { $0 == .immich }, googlePhotos: true, immich: true)
        #expect(onlyImmich.contains(.immich) && !onlyImmich.contains(.archive))
        let noService = SettingsPane.visible(isOn: { _ in true }, googlePhotos: false, immich: false)
        #expect(!noService.contains(.googlePhotos) && !noService.contains(.immich) && noService.contains(.archive))
    }

    @Test func everyPaneHasATitleAndSymbol() {
        #expect(SettingsPane.allCases.allSatisfy { !$0.title.isEmpty && !$0.symbol.isEmpty })
        #expect(Set(SettingsPane.allCases.map(\.title)).count == SettingsPane.allCases.count)
    }
}
