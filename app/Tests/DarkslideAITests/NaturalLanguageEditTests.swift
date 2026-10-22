import Testing
@testable import DarkslideAI
#if canImport(FoundationModels)
import FoundationModels
#endif

@Suite struct EditIntentTests {
    @Test func sanitizeClampsAndNormalizes() {
        let i = EditIntent(temperatureShift: 9000, tintShift: -99, exposureShift: .nan, contrastShift: 500,
                           highlightsShift: -500, shadowsShift: 50, saturationShift: 0, cropAspect: "2:1").sanitized()
        #expect(i.temperatureShift == 3000 && i.tintShift == -50 && i.exposureShift == 0)
        #expect(i.contrastShift == 100 && i.highlightsShift == -100 && i.shadowsShift == 50)
        #expect(i.cropAspect == "16:9")
        #expect(EditIntent(cropAspect: "none").sanitized().cropAspect == nil)
        #expect(EditIntent(cropAspect: "9:16").sanitized().cropAspect == "16:9")
    }

    @Test func newFieldsSanitizeAndIsEmptyIgnoresNotes() {
        var i = EditIntent()
        i.warmth = 500
        i.temperatureTarget = 90_000
        i.straightenDegrees = -80
        i.bandShifts = [BandShift(band: 5, saturation: 300, lightness: -5), BandShift(band: 9, saturation: 1, lightness: 1)]
        i.unrecognized = ["zorble"]
        let s = i.sanitized()
        #expect(s.warmth == 100 && s.temperatureTarget == 50_000 && s.straightenDegrees == -45)
        #expect(s.bandShifts == [BandShift(band: 5, saturation: 100, lightness: -5)])
        #expect(s.unrecognized == ["zorble"])
        var nan = EditIntent()
        nan.temperatureTarget = .nan
        nan.straightenDegrees = .infinity
        #expect(nan.sanitized().temperatureTarget == nil && nan.sanitized().straightenDegrees == nil)
        var notes = EditIntent()
        notes.unrecognized = ["hello"]
        #expect(notes.isEmpty && !s.isEmpty)
    }

    #if canImport(FoundationModels)
    @Test func generatedContentRoundTrips() throws {
        let i = EditIntent(temperatureShift: 800, exposureShift: 0.5, cropAspect: "4:5", straighten: true)
        let back = try EditIntent(i.generatedContent)
        #expect(back == i)
        let none = try EditIntent(EditIntent().generatedContent)
        #expect(none.cropAspect == "none", "schema uses the literal 'none'; sanitize maps it to nil")
        #expect(none.sanitized() == EditIntent())
        #expect(EditIntent.generationSchema.debugDescription.contains("temperatureShift"))
    }
    #endif
}

struct FakeEditService: NaturalLanguageEditService {
    var result: Result<EditIntent, NaturalLanguageEditError>
    func intent(for text: String) async throws -> EditIntent { try result.get() }
}

@Suite struct FallbackEditServiceTests {
    let rules = FakeEditService(result: .success(EditIntent(temperatureShift: 800)))

    @Test func usesPrimaryWhenItAnswers() async throws {
        let s = FallbackEditService(primary: FakeEditService(result: .success(EditIntent(exposureShift: 1))), fallback: rules)
        #expect(try await s.intent(for: "warmer").exposureShift == 1)
    }

    @Test func fallsBackOnErrorOrEmpty() async throws {
        let failing = FallbackEditService(primary: FakeEditService(result: .failure(.modelUnavailable("off"))), fallback: rules)
        #expect(try await failing.intent(for: "warmer").temperatureShift == 800)
        let empty = FallbackEditService(primary: FakeEditService(result: .success(EditIntent())), fallback: rules)
        #expect(try await empty.intent(for: "warmer").temperatureShift == 800)
        let onlyNotes = FallbackEditService(primary: FakeEditService(result: .success(EditIntent())), fallback: rules)
        #expect(try await onlyNotes.intent(for: "x").temperatureShift == 800)
    }
}

@Suite struct FoundationModelsEditTests {
    /// Real on-device model call. Skips (passes) when Apple Intelligence is off;
    /// asserts only the direction, since wording of the output varies by model version.
    @Test(.timeLimit(.minutes(2))) func warmerMeansPositiveTemperature() async throws {
        let service = FoundationModelsEditService()
        guard FoundationModelsEditService.isAvailable else {
            await #expect(throws: NaturalLanguageEditError.self) { try await service.intent(for: "warmer") }
            return
        }
        let i = try await service.intent(for: "make it much warmer")
        #expect(i.temperatureShift > 0)
        #expect(i == i.sanitized())
    }
}
