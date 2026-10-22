import dnssd
import Foundation

/// Advertises the link server on Bonjour, so an iPad on the LAN finds it with no setup.
public protocol LinkAdvertiser: AnyObject {
    /// Start advertising `name` at `port` (replacing an earlier advertisement); whether it worked.
    @discardableResult func advertise(name: String, port: UInt16) -> Bool
    func stop()
}

/// `LinkAdvertiser` on the macOS DNS-SD API (`DNSServiceRegister`), as
/// `_darkslide._tcp`. The registration lives until `stop()` releases it.
public final class DNSSDAdvertiser: LinkAdvertiser {
    public static let serviceType = "_darkslide._tcp"
    /// `kDNSServiceInterfaceIndexLocalOnly`: this Mac only (tests).
    static let localOnly = UInt32.max

    private let interface: UInt32
    private var ref: DNSServiceRef?

    /// `interface` 0 = every interface.
    public init(interface: UInt32 = 0) { self.interface = interface }

    deinit { stop() }

    public var isAdvertising: Bool { ref != nil }

    @discardableResult public func advertise(name: String, port: UInt16) -> Bool {
        stop()
        var r: DNSServiceRef?
        // No callback: dnssd needs none to register, and reports nothing we act on.
        let err = DNSServiceRegister(&r, 0, interface, name, Self.serviceType, nil, nil,
                                     port.bigEndian, 0, nil, nil, nil)
        guard err == DNSServiceErrorType(kDNSServiceErr_NoError), let r else { return false }
        ref = r
        return true
    }

    public func stop() {
        if let ref { DNSServiceRefDeallocate(ref) }
        ref = nil
    }
}
