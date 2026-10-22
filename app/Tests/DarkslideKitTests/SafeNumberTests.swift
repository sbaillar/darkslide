import Foundation
import Testing
@testable import DarkslideKit

@Suite("Untrusted numbers never trap")
struct SafeNumberTests {
    @Test func safeIntRoundsAndRejects() {
        #expect(Safe.int(2.6) == 3)
        #expect(Safe.int(-0.4) == 0)
        for bad in [Double.nan, .infinity, -.infinity, 1e30, -1e30] { #expect(Safe.int(bad) == nil) }
        #expect(Safe.clampedInt(1e30) == Int.max)
        #expect(Safe.clampedInt(-1e30) == Int.min)
        #expect(Safe.clampedInt(.nan) == 0)
    }

    @Test func jevScoreOutOfRangeIsIgnored() throws {
        let crit = JevEditService.criteria(JevEditService.questions[0])
        for score in [1e30, -1e30, 1e300] {
            #expect(JevEditService.level(["score": score], criteria: crit) == nil)
        }
        #expect(JevEditService.level(["score": 3.2], criteria: crit) == 3)
        let json = #"{"answers":{"warmth":{"score":1e30},"tint":{"score":-1e30}}}"#
        let intent = try JevEditService.intent(from: Data(json.utf8))
        #expect(intent.temperatureShift == 0 && intent.tintShift == 0)
    }

    @Test func presetWithHugeCropAspectDecodesWithoutTrapping() {
        for a in [1e30, -1e30, Double.nan, 1e19] {
            let e = EditCodec.decode(EditSnapshot(values: ["crop.aspect": a]))
            #expect(e.crop.aspect == EditState().crop.aspect)
        }
        let e = EditCodec.decode(EditSnapshot(values: ["exposure.ev": 1e300]))
        #expect(e.exposure.ev == EditState().exposure.ev)
    }

    @Test func formattersSurviveHugeValues() {
        #expect(Formatters.shutter(1e30).hasSuffix("\""))
        #expect(Formatters.shutter(1e-30).hasPrefix("1/"))
        #expect(Formatters.aperture(1e30).hasPrefix("f/"))
        #expect(Formatters.focal(1e30) == "– mm")
        #expect(Formatters.kelvin(1e30) == "– K")
        #expect(Formatters.duration(1e30).hasSuffix("min"))
        #expect(!Formatters.throughput(bytesPerSecond: 1e30).isEmpty)
        #expect(ValueFormat.integer.string(1e30) == String(Int.max))
        #expect(!ValueFormat.percent.string(1e30).isEmpty)
    }

    @Test func sidecarJSONWithHugeNumbersDecodesWithoutTrapping() throws {
        let json = #"{"exposure":{"exposure_ev":1e30},"crop":{"quarter_turns":1e30,"angle_deg":-1e300},"layers":[{"id":1e30,"opacity":1e30}]}"#
        let (e, _) = try AdjustmentsCodec.decode(json)
        #expect(e.crop.quarterTurns == 0 && e.crop.angle == 0, "out-of-range numbers read as absent")
        #expect(e.exposure.ev == 0)
        #expect(e.layers.first?.id == 0)
        let ok = try AdjustmentsCodec.decode(#"{"crop":{"quarter_turns":3}}"#).0
        #expect(ok.crop.quarterTurns == 3)
        // Past Double range (parsed as -inf), and negative huge ids, also read as absent.
        let overflow = #"{"crop":{"quarter_turns":-1e400},"layers":[{"id":-1e300},{"id":2}]}"#
        let o = try AdjustmentsCodec.decode(overflow).0
        #expect(o.crop.quarterTurns == 0)
        #expect(o.layers.map(\.id) == [0, 2])
        for bad in [Double.nan, .infinity, -.infinity, 1e30, -2e6] {
            #expect(AdjustmentsCodec.num(NSNumber(value: bad)) == nil, "\(bad)")
        }
        #expect(AdjustmentsCodec.num(NSNumber(value: -1e6)) == -1e6)
        #expect(AdjustmentsCodec.num(NSNumber(value: true)) == nil)
    }
}
