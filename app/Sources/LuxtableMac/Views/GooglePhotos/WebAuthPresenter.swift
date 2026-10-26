import AppKit
import AuthenticationServices
import LuxtableCore
import LuxtableUI

/// Shows Google's consent page in an `ASWebAuthenticationSession`. The redirect goes
/// to the loopback listener (Google allows only loopback redirects for Desktop
/// clients), not to a URL scheme, so the session is closed by `dismiss()` once the
/// listener has the code; it never completes on its own except when the user closes it.
@MainActor
final class WebAuthPresenter: NSObject, GooglePhotosAuthPresenter, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?

    func present(_ url: URL, onClose: @escaping @Sendable () -> Void) {
        let s = ASWebAuthenticationSession(url: url, callback: .customScheme("luxtable-google-photos")) { _, _ in onClose() }
        s.presentationContextProvider = self
        session = s
        s.start()
    }

    func dismiss() {
        session?.cancel()
        session = nil
    }

    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated { NSApp.keyWindow ?? NSApp.windows.first ?? NSWindow() }
    }
}
