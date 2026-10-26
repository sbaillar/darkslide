import LuxtableFFI
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore

/// The app's encoder (HEIC, hardware JPEG) writes Rust's XMP packet: the rating and the
/// hierarchical tags reach the file next to the IPTC fields.
@Suite struct ImmichEncoderTests {
    @Test func ratingAndTagsReachHEICAndJPEG() throws {
        let packet = """
            <x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">\
            <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" \
            xmlns:digiKam="http://www.digikam.org/ns/1.0/"><xmp:Rating>4</xmp:Rating>\
            <digiKam:TagsList><rdf:Seq><rdf:li>Luxtable/Pick</rdf:li></rdf:Seq></digiKam:TagsList>\
            </rdf:Description></rdf:RDF></x:xmpmeta>
            """
        let px = Data(repeating: 0x40, count: 4 * 4 * 6)
        for format in [EncodedImageFormat.jpeg, .heic] {
            let out = try ImageIOEncoder().encode(format: format, width: 4, height: 4, rgb16: px, quality: 80, icc: Data(),
                                                  exif: nil, keywords: ["Pick"],
                                                  metadata: RustSessionStore.record(IPTCMetadata()),
                                                  xmp: Data(packet.utf8))
            let src = try #require(CGImageSourceCreateWithData(out as CFData, nil))
            let meta = try #require(CGImageSourceCopyMetadataAtIndex(src, 0, nil))
            let data = try #require(CGImageMetadataCreateXMPData(meta, nil)) as Data
            let xmp = String(decoding: data, as: UTF8.self)
            #expect(xmp.contains("<xmp:Rating>4</xmp:Rating>"), "\(format)")
            #expect(xmp.contains("Luxtable/Pick"), "\(format)")
        }
    }
}
