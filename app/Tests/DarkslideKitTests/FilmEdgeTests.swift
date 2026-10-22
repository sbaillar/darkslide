import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The film-edge filmstrip line and sprocket holes, and the slider ruler's ticks.
@Suite struct FilmEdgeTests {
    private func photo(_ filename: String, rating: Int = 0, pick: PickState = .none, edited: Bool = false) -> Photo {
        let meta = PhotoMeta(camera: "Cam", lens: "", iso: 100, shutter: 0.01, aperture: 2.8, focalLength: 35,
                             captureDate: Date(timeIntervalSince1970: 0), pixelWidth: 6000, pixelHeight: 4000, fileSize: 1)
        var p = Photo(id: PhotoID(1), filename: filename, meta: meta, url: nil)
        p.rating = rating
        p.pick = pick
        p.isEdited = edited
        return p
    }

    @Test func frameNumberIsTheStemsLastFourDigits() {
        #expect(FilmEdge.frameNumber("DSC01005.ARW") == "1005")
        #expect(FilmEdge.frameNumber("IMG_7.JPG") == "7")
        #expect(FilmEdge.frameNumber("A9_2024_0001.ARW") == "0001")
        #expect(FilmEdge.frameNumber("DSC01005-2.ARW") == "2", "a variant suffix's digits win")
        #expect(FilmEdge.frameNumber("sunset.arw") == "SUNS", "no digits: the stem's start")
        #expect(FilmEdge.frameNumber("") == "")
    }

    @Test func captionPrintsOnlyWhatIsSet() {
        #expect(FilmEdge.caption(photo("DSC01005.ARW")) == "1005 ▸")
        #expect(FilmEdge.caption(photo("DSC01005.ARW", rating: 3, edited: true)) == "1005 ▸ ★★★ EDITED")
        #expect(FilmEdge.caption(photo("DSC01006.ARW", rating: 5, pick: .picked)) == "1006 ▸ ★★★★★ PICK")
        #expect(FilmEdge.caption(photo("DSC01007.ARW", pick: .rejected, edited: true)) == "1007 ▸ REJECT EDITED")
        #expect(FilmEdge.caption(photo("X1.ARW", rating: 9)) == "1 ▸ ★★★★★", "never more than five stars")
    }

    @Test func sprocketHolesAreEvenAndCentred() {
        let xs = FilmEdge.sprocketXs(width: 100)
        #expect(xs.count == 7)
        #expect(zip(xs, xs.dropFirst()).allSatisfy { abs($1 - $0 - 14) < 1e-9 })
        let left = xs[0], right = 100 - (xs[xs.count - 1] + 6)
        #expect(abs(left - right) < 1e-9, "trimmed alike at both ends")
        #expect(FilmEdge.sprocketXs(width: 6) == [0])
        #expect(FilmEdge.sprocketXs(width: 5).isEmpty)
        #expect(FilmEdge.sprocketXs(width: 100, pitch: 0).isEmpty)
    }

    @Test func rulerEndsAndMiddleAreMajorTicks() {
        for width in [40.0, 150, 163, 400] {
            let ticks = SliderRuler.ticks(width: width)
            #expect(ticks.first == .init(fraction: 0, isMajor: true))
            #expect(ticks.last == .init(fraction: 1, isMajor: true))
            #expect(ticks.contains(.init(fraction: 0.5, isMajor: true)), "a bipolar default sits on a major tick")
            let spacing = width / Double(ticks.count - 1)
            #expect(spacing > 2.5 && spacing < 10, "fine but legible at \(width)pt")
        }
        #expect(SliderRuler.ticks(width: 150).count == 31)
        #expect(SliderRuler.ticks(width: 0).isEmpty)
        #expect(SliderRuler.ticks(width: 100, majorEvery: 0).isEmpty)
    }
}
