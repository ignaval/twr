import Capacitor
import UIKit

final class TwrViewController: CAPBridgeViewController {
    override func capacitorDidLoad() {
        bridge?.registerPluginInstance(TwrTransportPlugin())
        bridge?.registerPluginInstance(TwrLayoutPlugin())
    }
}

// Read reserved regions during updateConstraints: UIKit tracks those reads and
// invalidates this view when the fold or camera changes, even without a resize.
// Build with the iOS 27.1 SDK; deployment requires iOS 17.4 and later, matching the web UI target.
private final class RegionObserver: UIView {
    override class var requiresConstraintBasedLayout: Bool { true }
    private var previousBounds = CGRect.null
    var changed: (([String: Any]) -> Void)?
    override func updateConstraints() {
        super.updateConstraints()
        var result: [String: Any] = ["orientation": "none", "occlusionTop": 0]
        if #available(iOS 27.1, *) {
            if let region = reservedRegions(kind: .division).first {
                let frame = region.frame
                result = ["orientation": frame.height > frame.width ? "vertical" : "horizontal",
                          "state": "half-opened", "separating": true,
                          "left": frame.minX, "top": frame.minY,
                          "width": frame.width, "height": frame.height]
            }
            let top = reservedRegions(kind: .occlusion).map(\.frame)
                .filter { $0.minY <= safeAreaInsets.top + 1 }
                .map(\.maxY).max() ?? 0
            result["occlusionTop"] = top
        }
        changed?(result)
    }
    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        setNeedsUpdateConstraints()
    }
    override func layoutSubviews() {
        super.layoutSubviews()
        if bounds != previousBounds {
            previousBounds = bounds
            setNeedsUpdateConstraints()
        }
    }
}

@objc(TwrLayoutPlugin)
final class TwrLayoutPlugin: CAPPlugin, CAPBridgedPlugin {
    let identifier = "TwrLayoutPlugin"
    let jsName = "TwrLayout"
    let pluginMethods: [CAPPluginMethod] = [CAPPluginMethod(name: "current", returnType: CAPPluginReturnPromise)]
    private var observer: RegionObserver?
    private var layout: [String: Any] = ["orientation": "none"]
    override func load() {
        DispatchQueue.main.async { [weak self] in
            guard let self, let webView = self.bridge?.webView else { return }
            let observer = RegionObserver(frame: webView.bounds)
            observer.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            observer.isUserInteractionEnabled = false
            observer.backgroundColor = .clear
            observer.changed = { [weak self] next in
                guard let self, !NSDictionary(dictionary: self.layout).isEqual(to: next) else { return }
                self.layout = next
                self.notifyListeners("change", data: next, retainUntilConsumed: true)
            }
            self.observer = observer
            webView.addSubview(observer)
            observer.setNeedsUpdateConstraints()
        }
    }
    @objc func current(_ call: CAPPluginCall) {
        DispatchQueue.main.async { [weak self] in call.resolve(self?.layout ?? ["orientation": "none"]) }
    }
}
