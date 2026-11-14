import XCTest
import WebKit
import UIKit
import UserNotifications
@testable import App

/// Runs in the real Simulator app, through its WKWebView and native plugins.
@MainActor
final class RemoteHubTests: XCTestCase {
    private var web: WKWebView!

    private func js(_ body: String, _ arguments: [String: Any] = [:]) async throws -> Any? {
        try await web.callAsyncJavaScript(body, arguments: arguments, in: nil, contentWorld: .page)
    }

    private func until(_ body: String, seconds: Double = 30, reportFailure: Bool = true) async throws {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            let check = """
              return await Promise.race([(async()=>{\(body)})(),new Promise(resolve=>setTimeout(()=>resolve(false),3000))]);
              """
            if (try? await js(check)) as? Bool == true { return }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        let html = (try? await js("return document.body.innerText")) as? String ?? "No document"
        if reportFailure { XCTFail("Timed out: \(body)\n\(html.prefix(3000))") }
        throw NSError(domain: "twr.qa", code: 1, userInfo: [NSLocalizedDescriptionKey: "Timed out: \(body)"])
    }

    private func dismissKeyboard() async throws {
        let control = try await js("""
          const button=document.querySelector('button[aria-label="Dismiss keyboard"]');
          if(button?.getClientRects().length){
            const box=button.getBoundingClientRect();
            return box.width>=44 && box.height>=44 && button.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2));
          }
          return null;
          """) as? Bool
        if let control {
            XCTAssertTrue(control, "Dismiss keyboard must be a reachable 44-point control")
            _ = try await js("document.querySelector('button[aria-label=\"Dismiss keyboard\"]').click();return true")
        } else {
            // Pairing and terminal screens may have no composer control.
            _ = try await js("document.activeElement?.blur();await window.Capacitor.Plugins.Keyboard.hide();return true")
        }
        // Native didHide schedules a final viewport read after 250 ms. Wait for
        // that event first, then let its settled layout update complete.
        try await until("return document.documentElement.dataset.keyboard !== 'true'")
        try await Task.sleep(nanoseconds: 350_000_000)
    }

    private func assertViewportCoordinates(_ stage: String) async throws {
        let deadline = Date().addingTimeInterval(10)
        var latest: Any?
        while Date() < deadline {
            latest = try await js("""
              const view=window.visualViewport,root=document.getElementById('root').getBoundingClientRect();
              return {pass:!!view && view.scale===1 && view.offsetTop===0 && view.offsetLeft===0 && Math.abs(innerWidth-nativeWidth)<=1 && root.width>=44 && root.height>=160,viewport:{width:innerWidth,height:innerHeight,visualWidth:view?.width,visualHeight:view?.height,scale:view?.scale,offsetLeft:view?.offsetLeft,offsetTop:view?.offsetTop},nativeWidth,nativeHeight,root:root.toJSON()};
              """, ["nativeWidth": Double(web.bounds.width), "nativeHeight": Double(web.bounds.height)])
            if (latest as? [String: Any])?["pass"] as? Bool == true {
                print("VIEWPORT_COORDINATES pass stage=\(stage) " + String(describing: latest))
                return
            }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTFail("Native CGRect and CSS coordinates are not comparable, or the content root collapsed: \(stage): \(String(describing: latest))")
        throw NSError(domain: "twr.qa.coordinates", code: 1)
    }

    private func assertComposerReachable(_ stage: String) async throws {
        try await assertViewportCoordinates(stage)
        let check = """
          const button=document.querySelector('button[aria-label="Send message"]'),box=button?.getBoundingClientRect();
          const viewport=window.visualViewport,tabs=document.querySelector('.mobile-tabs');
          const limit=Math.min(innerHeight,viewport?.height||innerHeight,tabs?.getClientRects().length?tabs.getBoundingClientRect().top:Infinity);
          return !!box && box.width>=44 && box.height>=44 && box.left>=0 && box.top>=0 && box.right<=innerWidth+1 && box.bottom<=limit+1 && button.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2));
          """
        do {
            try await until(check, seconds: 10, reportFailure: false)
        } catch {
            print("POSE_COMPOSER failure stage=\(stage) " + String(describing: try? await js("""
              const button=document.querySelector('button[aria-label="Send message"]'),box=button?.getBoundingClientRect();
              const hit=box?document.elementFromPoint(box.x+box.width/2,box.y+box.height/2):null;
              return JSON.stringify({viewport:{width:innerWidth,height:innerHeight,visualHeight:visualViewport?.height},root:document.getElementById('root').getBoundingClientRect().toJSON(),send:box?.toJSON(),tabs:document.querySelector('.mobile-tabs')?.getBoundingClientRect().toJSON(),hit:hit?{tag:hit.tagName,aria:hit.getAttribute('aria-label')}:null,layout:await window.Capacitor.Plugins.TwrLayout.current()});
              """)))
            XCTFail("Send must be a visible, unobstructed 44-point target after fold pose: " + stage)
            throw error
        }
        print("POSE_COMPOSER pass stage=" + stage)
    }

    private func assertContentClearOfOcclusions(_ stage: String, requireOcclusions: Bool = false) async throws {
        try await assertViewportCoordinates(stage)
        let check = """
          const layout=await window.Capacitor.Plugins.TwrLayout.current();
          const root=document.getElementById('root').getBoundingClientRect();
          const regions=layout.occlusions;
          if(!Array.isArray(regions) || (\(requireOcclusions ? "true" : "false") && !regions.length))return false;
          return regions.every(region=>{
            const width=Math.max(0,Math.min(root.right,region.left+region.width)-Math.max(root.left,region.left));
            const height=Math.max(0,Math.min(root.bottom,region.top+region.height)-Math.max(root.top,region.top));
            return width===0 || height===0;
          });
          """
        do {
            try await until(check, seconds: 10, reportFailure: false)
        } catch {
            print("OCCLUSIONS failure stage=\(stage) " + String(describing: try? await js("""
              return JSON.stringify({layout:await window.Capacitor.Plugins.TwrLayout.current(),root:document.getElementById('root').getBoundingClientRect().toJSON(),bodyPadding:getComputedStyle(document.body).padding,viewport:{width:innerWidth,height:innerHeight,offsetTop:visualViewport?.offsetTop,offsetLeft:visualViewport?.offsetLeft,scale:visualViewport?.scale}});
              """)))
            XCTFail("Content overlaps a native reserved occlusion or required regions are missing: " + stage)
            throw error
        }
        print("OCCLUSIONS pass stage=\(stage) " + String(describing: try? await js("""
          return JSON.stringify({layout:await window.Capacitor.Plugins.TwrLayout.current(),root:document.getElementById('root').getBoundingClientRect().toJSON(),bodyPadding:getComputedStyle(document.body).padding});
          """)))
    }

    private func appearanceDiagnostics(_ stage: String, controller: TwrViewController) async -> String {
        let state = try? await js("""
          const root=document.documentElement,colors=getComputedStyle(root);
          return JSON.stringify({theme:root.dataset.theme,scheme:colors.colorScheme,background:colors.getPropertyValue('--bg').trim(),prefersDark:matchMedia('(prefers-color-scheme: dark)').matches,prefersLight:matchMedia('(prefers-color-scheme: light)').matches,trace:window.__qaAppearanceTrace||[]});
          """)
        return "stage=\(stage) status=\(controller.preferredStatusBarStyle.rawValue) trait=\(controller.traitCollection.userInterfaceStyle.rawValue) override=\(controller.overrideUserInterfaceStyle.rawValue) view=\(String(describing:controller.view.backgroundColor)) web=\(String(describing:web.backgroundColor)) state=\(String(describing:state))"
    }

    private func statusBar(_ expected: UIStatusBarStyle, stage: String) async throws {
        let controller = try XCTUnwrap(web.window?.rootViewController as? TwrViewController)
        print("APPEARANCE start expected=\(expected.rawValue) " + (await appearanceDiagnostics(stage, controller: controller)))
        let backgroundValue = try await js("return getComputedStyle(document.documentElement).getPropertyValue('--bg').trim().slice(1)") as? String
        let background = try XCTUnwrap(backgroundValue)
        let rgb = try XCTUnwrap(UInt32(background, radix: 16))
        let expectedColor = UIColor(red: CGFloat((rgb >> 16) & 255) / 255,
                                    green: CGFloat((rgb >> 8) & 255) / 255,
                                    blue: CGFloat(rgb & 255) / 255, alpha: 1)
        for _ in 0..<100 {
            if controller.preferredStatusBarStyle == expected && controller.view.backgroundColor == expectedColor && web.backgroundColor == expectedColor {
                print("APPEARANCE pass expected=\(expected.rawValue) " + (await appearanceDiagnostics(stage, controller: controller)))
                return
            }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        let diagnostic = await appearanceDiagnostics(stage, controller: controller)
        print("APPEARANCE fail expected=\(expected.rawValue) " + diagnostic)
        XCTAssertEqual(controller.preferredStatusBarStyle, expected, "Native status-bar contrast must match the rendered theme: " + diagnostic)
        XCTAssertEqual(controller.view.backgroundColor, expectedColor, "Native background must match the rendered theme: " + stage)
        XCTAssertEqual(web.backgroundColor, expectedColor, "WebView background must match the rendered theme: " + stage)
    }

    private func verifyLocalNotificationDelivery() async throws {
        let center = UNUserNotificationCenter.current()
        // Provisional authorization is public, quiet and does not show a
        // permission prompt. It needs neither APNs credentials nor entitlements.
        let granted = try await center.requestAuthorization(options: [.alert, .sound, .badge, .provisional])
        XCTAssertTrue(granted, "The disposable Simulator must permit provisional local notifications")
        let settings = await center.notificationSettings()
        XCTAssertTrue(settings.authorizationStatus == .provisional || settings.authorizationStatus == .authorized,
                      "Notification authorization must be granted before exercising delivery")
        _ = try await js("""
          window.__qaLocalNotification=null;
          window.__qaLocalNotificationListener=await window.Capacitor.Plugins.LocalNotifications.addListener('localNotificationReceived',notification=>{window.__qaLocalNotification=notification});
          document.querySelector('button[aria-label^="Remote hub settings:"]').click();return true;
          """)
        try await until("return [...document.querySelectorAll('[role=dialog] button')].some(button=>button.textContent.trim()==='Test notification')")
        _ = try await js("[...document.querySelectorAll('[role=dialog] button')].find(button=>button.textContent.trim()==='Test notification').click();return true")
        try await until("return window.__qaLocalNotification?.title==='twr · local test' && window.__qaLocalNotification?.extra?.event==='test'", seconds: 20)
        let identifierValue = try await js("return String(window.__qaLocalNotification.id)") as? String
        let identifier = try XCTUnwrap(identifierValue)
        var delivered: UNNotification?
        for _ in 0..<100 {
            delivered = await center.deliveredNotifications().first { $0.request.identifier == identifier }
            if delivered != nil { break }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        let actual = try XCTUnwrap(delivered, "The notification received through the native bridge must also exist in Notification Center")
        XCTAssertEqual(actual.request.content.title, "twr · local test")
        XCTAssertTrue(actual.request.content.body.contains("Tap to open Needs you"))
        let extra = try XCTUnwrap(actual.request.content.userInfo["cap_extra"] as? [String: Any])
        XCTAssertEqual(extra["event"] as? String, "test")
        print("PASS: real iOS local notification delivered to Notification Center and emitted by the native plugin; provisional authorization=\(settings.authorizationStatus.rawValue) id=\(identifier)")
        _ = try await js("await window.__qaLocalNotificationListener.remove();document.querySelector('[aria-label=\"Close dialog\"]').click();return true")
        center.removeDeliveredNotifications(withIdentifiers: [identifier])
    }

    private func capture(_ name: String) async throws {
        web.layoutIfNeeded()
        let bounds = try await js("""
          await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);
          const finite=document.getAnimations().filter(animation=>animation.playState==='running' && animation.effect && Number.isFinite(animation.effect.getComputedTiming().endTime));
          let timer;
          try {await Promise.race([Promise.all(finite.map(animation=>animation.finished.catch(()=>{}))),new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('Finite UI animation did not settle before capture')),5000)})])}finally{clearTimeout(timer)}
          await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);
          const box=selector=>document.querySelector(selector)?.getBoundingClientRect().toJSON();
          return JSON.stringify({name,viewport:{width:innerWidth,height:innerHeight,visualHeight:visualViewport?.height,offsetTop:visualViewport?.offsetTop,offsetLeft:visualViewport?.offsetLeft,scale:visualViewport?.scale},root:box('#root'),send:box('button[aria-label="Send message"]'),tabs:box('.mobile-tabs'),bodyPadding:getComputedStyle(document.body).padding,keyboard:document.documentElement.dataset.keyboard,fold:document.documentElement.dataset.fold});
          """, ["name":name])
        let configuration = WKSnapshotConfiguration()
        configuration.afterScreenUpdates = true
        let image: UIImage = try await withCheckedThrowingContinuation { continuation in
            web.takeSnapshot(with: configuration) { image, error in
                if let error { continuation.resume(throwing: error) }
                else if let image { continuation.resume(returning: image) }
                else { continuation.resume(throwing: NSError(domain: "twr.qa.snapshot", code: 1)) }
            }
        }
        // Preserve the native snapshot in the test result before exporting a
        // convenient PNG. A stalled local artifact upload must not skip UI QA.
        let attachment = XCTAttachment(image: image)
        attachment.name = "twr-" + name
        attachment.lifetime = .keepAlways
        add(attachment)
        print("UI_BOUNDS " + String(describing: bounds))
        var request = URLRequest(url: URL(string: "http://127.0.0.1:47423/screenshot?name=" + name)!, timeoutInterval: 15)
        request.httpMethod = "POST"
        request.httpBody = try XCTUnwrap(image.pngData())
        request.setValue("image/png", forHTTPHeaderField: "Content-Type")
        request.setValue("close", forHTTPHeaderField: "Connection")
        do {
            let (_, response) = try await URLSession.shared.data(for: request)
            if (response as? HTTPURLResponse)?.statusCode != 200 {
                print("Snapshot PNG export failed; native image remains in XCTest attachments: " + name)
            }
        } catch {
            print("Snapshot PNG export failed; native image remains in XCTest attachments: \(name): \(error)")
        }
    }

    private func pose(_ name:String) async throws {
        let request=URLRequest(url:URL(string:"http://127.0.0.1:47423/pose/"+name)!,timeoutInterval:240)
        let (data,response)=try await URLSession.shared.data(for:request)
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode,200,String(data:data,encoding:.utf8) ?? "Pose failed")
    }
    /// Focused native typography regression; the host only changes this QA clone's OS setting.
    func testNativeTextScale() async throws {
        continueAfterFailure = false
        var ready = URLRequest(url: URL(string: "http://127.0.0.1:47330/ready")!); ready.timeoutInterval = 1
        guard let (_, fixtureResponse) = try? await URLSession.shared.data(for: ready), (fixtureResponse as? HTTPURLResponse)?.statusCode == 200 else { throw XCTSkip("Run the focused native-font QA fixture") }
        for _ in 0..<100 {
            let controller = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .flatMap(\.windows).compactMap { $0.rootViewController as? TwrViewController }.first
            web = controller?.webView
            if web != nil { break }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTAssertNotNil(web)
        try await until("return !!document.querySelector('.mobile-onboarding') && !!window.Capacitor?.Plugins?.TwrOffline")
        for category in ["large", "accessibility-extra-large"] {
            let (_, response) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47330/font?category=" + category)!)
            XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200)
            try await until("""
              const native=await window.Capacitor.Plugins.TwrOffline.textScale();
              const size=parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--text-size'));
              return native.scale >= \(category == "large" ? 0.9 : 1.8) && Math.abs(size-Math.min(44,13*Math.min(2.25,Math.max(.8,native.scale))))<.1;
              """, seconds: 15)
            let native = try await js("return await window.Capacitor.Plugins.TwrOffline.textScale()") as? [String: Any]
            let scale = (native?["scale"] as? Double) ?? 0
            if category == "large" { XCTAssertEqual(scale, 1, accuracy: 0.05) }
            else { XCTAssertGreaterThan(scale, 1.8) }
            let diagnostics = try await js("""
              const root=document.documentElement,style=getComputedStyle(root);
              const controls=[...document.querySelectorAll('.mobile-onboarding button,.mobile-onboarding input')].filter(e=>e.getClientRects().length);
              const bounds=controls.map(e=>({label:e.getAttribute('aria-label')||e.textContent||e.name,box:e.getBoundingClientRect().toJSON()}));
              return {native:await window.Capacitor.Plugins.TwrOffline.textScale(),size:style.fontSize,css:style.getPropertyValue('--text-size'),width:innerWidth,scroll:document.body.scrollWidth,controls:bounds};
              """)
            print("NATIVE_TEXT_SCALE category=\(category) " + String(describing: diagnostics))
            _ = try await js("[...document.querySelectorAll('button')].find(e=>e.textContent==='Pair a hub').click();return true")
            try await until("return !!document.querySelector('input[name=code]')")
            let targets = try await js("""
              const controls=[...document.querySelectorAll('.pair-form button,.pair-form input')].filter(e=>e.getClientRects().length);
              const bad=[];for(const e of controls){e.scrollIntoView({block:'center'});const box=e.getBoundingClientRect();if(box.height<43.5||box.width<43.5||box.left<-.5||box.right>innerWidth+.5)bad.push({label:e.textContent||e.name,box:box.toJSON()})}
              return bad;
              """) as? [Any]
            XCTAssertEqual(targets?.count, 0, "Every pairing target must remain reachable at \(category): \(String(describing: targets))")
            _ = try await js("[...document.querySelectorAll('.pair-form button')].find(e=>e.textContent==='Cancel').click();return true")
        }
    }

    /// Review loops on the phone (mobile/scripts/loop-qa.mjs). The host drives: this test
    /// runs what it is sent in the app's WKWebView (or turns the screen) and answers, until
    /// the host says done. It runs after testRemoteHubEndToEnd, on the app that test paired.
    func testReviewLoops() async throws {
        continueAfterFailure = false
        executionTimeAllowance = 1800
        var begin = URLRequest(url: URL(string: "http://127.0.0.1:47423/loop/begin")!)
        begin.timeoutInterval = 5
        guard let (_, beginResponse) = try? await URLSession.shared.data(for: begin),
              (beginResponse as? HTTPURLResponse)?.statusCode == 200
        else { throw XCTSkip("Run mobile/scripts/ios-qa.mjs for the review-loop QA") }
        for _ in 0..<100 {
            let controller = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .flatMap(\.windows).compactMap { $0.rootViewController as? TwrViewController }.first
            web = controller?.webView
            if web != nil { break }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTAssertNotNil(web)
        while true {
            var next = URLRequest(url: URL(string: "http://127.0.0.1:47423/loop/next")!)
            next.timeoutInterval = 1200
            let (data, _) = try await URLSession.shared.data(for: next)
            let command = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
            var reply: [String: Any] = ["id": command["id"] ?? 0]
            switch command["op"] as? String {
            case "done":
                return
            case "fail":
                XCTFail(command["message"] as? String ?? "Review-loop QA failed")
                return
            case "js":
                let source = command["source"] as? String ?? "async () => null"
                // The page answers in JSON, its own exceptions included, so the host sees them.
                let value = try? await js("""
                  try { const value = await (\(source))(JSON.parse(arg)); return JSON.stringify({ value: value === undefined ? null : value }); }
                  catch (error) { return JSON.stringify({ error: error?.message ? error.message + "\\n" + (error.stack || "") : String(error) }); }
                  """, ["arg": command["arg"] as? String ?? "null"])
                reply["json"] = value as? String ?? "{\"error\":\"The page did not answer\"}"
            case "orient":
                let landscape = command["value"] as? String == "landscape"
                let scene = try XCTUnwrap(web.window?.windowScene)
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: landscape ? .landscapeRight : .portrait)) { error in
                    print("ORIENTATION_FAILED \(error)")
                }
                web.window?.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
                try await Task.sleep(nanoseconds: 1_500_000_000)
                reply["json"] = "{\"value\":true}"
            default:
                reply["json"] = "{\"error\":\"Unknown command\"}"
            }
            var result = URLRequest(url: URL(string: "http://127.0.0.1:47423/loop/result")!, timeoutInterval: 30)
            result.httpMethod = "POST"
            result.httpBody = try JSONSerialization.data(withJSONObject: reply)
            result.setValue("application/json", forHTTPHeaderField: "Content-Type")
            _ = try await URLSession.shared.data(for: result)
        }
    }

    func testRemoteHubEndToEnd() async throws {
        continueAfterFailure = false
        for _ in 0..<100 {
            let controller = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .flatMap(\.windows).compactMap { $0.rootViewController as? TwrViewController }.first
            web = controller?.webView
            if web != nil { break }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTAssertNotNil(web)
        let (data, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47423/fixture")!)
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let code = try XCTUnwrap(fixture["code"] as? String)
        let hubURL = try XCTUnwrap(fixture["url"] as? String)
        let repo = try XCTUnwrap(fixture["repo"] as? String)
        let expectedCatalogIds = try XCTUnwrap(fixture["catalogIds"] as? [String])
        if fixture["foldable"] as? Bool == true {try await pose("closed")}

        try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent==='Pair a hub')")
        try await assertContentClearOfOcclusions("initial", requireOcclusions: fixture["foldable"] as? Bool == true)
        // Trace only appearance requests and rendered theme state. Device
        // credentials and ordinary hub calls are deliberately not recorded.
        _ = try await js("""
          window.__qaAppearanceTrace=[];
          const state=()=>{const root=document.documentElement,style=getComputedStyle(root);return {theme:root.dataset.theme,scheme:style.colorScheme,prefersDark:matchMedia('(prefers-color-scheme: dark)').matches,prefersLight:matchMedia('(prefers-color-scheme: light)').matches,background:style.getPropertyValue('--bg').trim()}};
          const record=(phase,extra={})=>{window.__qaAppearanceTrace.push({phase,time:performance.now(),...state(),...extra});if(window.__qaAppearanceTrace.length>48)window.__qaAppearanceTrace.shift()};
          const original=window.Capacitor.nativePromise;
          window.Capacitor.nativePromise=(...args)=>{const [plugin,method,options]=args,result=original.apply(window.Capacitor,args);if((plugin==='SystemBars'&&method==='setStyle')||(plugin==='TwrLayout'&&method==='setAppearance')){record('request',{plugin,method,options});void Promise.resolve(result).then(()=>record('reply',{plugin,method,options}),error=>record('error',{plugin,method,message:String(error)}))}return result};
          const media=[];for(const query of ['(prefers-color-scheme: dark)','(prefers-color-scheme: light)']){const match=matchMedia(query),listener=()=>{record('media',{query});requestAnimationFrame(()=>record('media-frame',{query}))};match.addEventListener('change',listener);media.push([match,listener])}
          const observer=new MutationObserver(()=>record('theme'));observer.observe(document.documentElement,{attributes:true,attributeFilter:['data-theme']});
          window.__qaStopAppearanceTrace=()=>{window.Capacitor.nativePromise=original;for(const [match,listener] of media)match.removeEventListener('change',listener);observer.disconnect()};
          record('initial');return true;
          """)
        // Verify native contrast independently of the OS's current appearance.
        let controller = try XCTUnwrap(web.window?.rootViewController as? TwrViewController)
        controller.overrideUserInterfaceStyle = .light
        try await until("return document.documentElement.dataset.theme === 'dark'")
        try await statusBar(.lightContent, stage: "dark-on-light-os")
        _ = try await js("document.documentElement.dataset.theme='light';return true")
        try await statusBar(.darkContent, stage: "explicit-light")
        _ = try await js("document.documentElement.dataset.theme='system';return true")
        try await until("return getComputedStyle(document.documentElement).colorScheme === 'light'")
        try await statusBar(.darkContent, stage: "system-light")
        controller.overrideUserInterfaceStyle = .dark
        try await until("return getComputedStyle(document.documentElement).colorScheme === 'dark'")
        try await statusBar(.lightContent, stage: "system-dark")
        controller.overrideUserInterfaceStyle = .unspecified
        _ = try await js("document.documentElement.dataset.theme='dark';return true")
        try await statusBar(.lightContent, stage: "restored-dark")
        print("PASS: native status-bar contrast follows Dark, Light, System and native trait changes")
        _ = try await js("window.__qaStopAppearanceTrace();return true")
        _ = try await js("""
          window.__qaErrors=[];window.addEventListener('error',e=>window.__qaErrors.push(e.message));
          [...document.querySelectorAll('button')].find(e=>e.textContent==='Pair a hub').click();return true;
          """)
        try await until("return !!document.querySelector('input[name=code]')")
        _ = try await js("""
          const values={hubName:'Simulator QA',url:hubURL,code,name:'iOS Simulator'};
          for(const [key,value] of Object.entries(values)){
            const input=document.querySelector('input[name="'+key+'"]');
            Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,value);
            input.dispatchEvent(new Event('input',{bubbles:true}));
          }
          document.querySelector('.pair-form').requestSubmit();return true;
          """, ["code": code,"hubURL": hubURL])
        try await until("try{return (await window.twr.rpc('hub.info')).identity.startsWith('user:device:')}catch{return false}", seconds: 120)
        let noToken = try await js("return !JSON.stringify(await window.Capacitor.Plugins.TwrTransport.listHubs()).includes('\"token\"')")
        XCTAssertEqual(noToken as? Bool, true, "Keychain credentials must stay native")
        _ = try await js("""
          window.__qaEvents=[];window.twr.onEvent(e=>window.__qaEvents.push(e));
          await window.twr.rpc('thread.new',{project:'demo',name:'ios-worker',runtime:'fake'});
          await window.twr.rpc('thread.send',{thread:'ios-worker',prompt:'IOS_REMOTE_STREAM_OK'});return true;
          """)
        try await dismissKeyboard()
        _ = try await js("""
          const visible=e=>e.getClientRects().length>0;
          const nav=[...document.querySelectorAll('.mobile-tabs button,.primary-nav button')]
            .find(e=>visible(e)&&(e.textContent==='Threads'||e.textContent.includes('Thread grid')));nav.click();return true;
          """)
        try await until("return !![...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker'))")
        _ = try await js("[...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker')).click();return true")
        try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread'))")
        _ = try await js("[...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread')).click();return true")
        try await until("return [...document.querySelectorAll('.block-assistant .markdown')].some(e=>e.textContent.includes('IOS_REMOTE_STREAM_OK')) && !!document.querySelector('.composer textarea')")
        _ = try await js("""
          const path=await window.twr.uploadFile(new File(['IOS_UPLOAD_OK'],'ios-qa.txt',{type:'text/plain'}),'ios-worker');
          window.__qaUpload=path.path;return true;
          """)
        let uploaded = try await js("return window.__qaUpload") as? String
        XCTAssertTrue(uploaded?.contains(".twr-uploads/") == true)
        // A separate fixture endpoint verifies the uploaded bytes on the remote host.
        let (verification, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47423/verify-upload")!)
        XCTAssertEqual(String(data: verification, encoding: .utf8), "IOS_UPLOAD_OK")
        _ = try await js("""
          await window.twr.rpc('thread.send',{thread:'ios-worker',prompt:'!ask Continue iOS QA?|Proceed|Wait\\nIOS_ANSWERED'});return true;
          """)
        try await until("return JSON.stringify(window.__qaEvents).includes('Continue iOS QA?')")
        _ = try await js("""
          [...document.querySelectorAll('.mobile-tabs button,.primary-nav button')]
           .find(e=>e.getClientRects().length&&e.textContent.includes('Needs you')).click();return true;
          """)
        // A request's options answer only once they have been on screen for 600 ms (armed).
        try await until("return !![...document.querySelectorAll('.attention-card button')].find(e=>e.textContent.includes('Proceed')&&e.closest('.attention-options')?.getAttribute('data-armed')==='true')")
        _ = try await js("[...document.querySelectorAll('.attention-card button')].find(e=>e.textContent.includes('Proceed')).click();return true")
        try await until("return window.__qaEvents.some(e=>e.type==='thread' && e.event?.kind==='agent.text' && e.event?.data?.text?.includes('IOS_ANSWERED'))")
        let commandIds = try await js("return (await window.twr.catalog()).map(command => command.id).sort()") as? [String]
        XCTAssertEqual(commandIds, expectedCatalogIds.sorted(), "The mobile catalog must match the generated CLI commands")
        let sizeGuard = try await js("""
          try { await window.Capacitor.Plugins.TwrTransport.send({message:'界'.repeat(750000)}); return false }
          catch(error) { return error.code==='REQUEST_TOO_LARGE' }
          """) as? Bool
        XCTAssertEqual(sizeGuard, true, "Native requests must be bounded by UTF-8 bytes without closing the socket")
        // Back to the conversation: the Chat tab on a phone, the open workspace's Orchestrator
        // entry in the sidebar's workspace list on a wide layout.
        _ = try await js("[...document.querySelectorAll('.mobile-tabs button,.workspace-entry.current .workspace-orchestrator,.primary-nav button')].find(e=>e.getClientRects().length&&(e.textContent==='Chat'||e.textContent.includes('Orchestrator'))).click();return true")
        try await until("return !!document.querySelector('.composer textarea')")
        _ = try await js("const shell=[...document.querySelectorAll('button[aria-label=\"Open shell\"]')].find(e=>e.getClientRects().length);if(shell)shell.click();else{document.querySelector('.conversation-menu summary').click();[...document.querySelectorAll('.conversation-menu button')].find(e=>e.textContent.includes('Open shell')).click();}return true")
        try await until("return !!document.querySelector('.terminal-session .xterm-helper-textarea') && !!document.querySelector('.terminal-tab.active button[aria-label^=\"Close \"]')")
        let (fontScript, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47423/typography-script")!)
        let typography = try await js(String(decoding: fontScript, as: UTF8.self)) as? [String: Any]
        XCTAssertEqual(typography?["pass"] as? Bool, true, "Connected large-text terminal and composer must remain usable")
        print("CONNECTED_TYPOGRAPHY " + String(describing: typography))
        _ = try await js("""
          window.__qaTerminal={id:document.querySelector('.terminal-session').dataset.sessionId};
          const command="printf 'ORDER_%s\\\\n' '0123456789abcdefghijklmnopqrstuvwxyz'\\r";
          for(const char of command)window.twr.terminalWrite(window.__qaTerminal.id,char);return true;
          """)
        try await until("return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')")
        // Reset the real TCP connection, keeping the gateway and its PTY alive.
        _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47423/drop-socket")!)
        try await until("try{return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')}catch{return false}", seconds: 45)
        print("RECONNECT_PASS ordered PTY survived socket reset")
        // Close through the app so both the remote PTY and its local pane disappear.
        // Calling the host API alone intentionally leaves the terminal's UI history open.
        _ = try await js("document.querySelector('.terminal-tab.active button[aria-label^=\"Close \"]').click();return true")
        try await until("return !!document.querySelector('[role=dialog]')")
        _ = try await js("[...document.querySelectorAll('[role=dialog] button')].find(e=>['Close terminal','Remove tab'].includes(e.textContent.trim())).click();return true")
        try await until("return !document.querySelector('.terminal-dock')")
        _ = try await js("await window.Capacitor.Plugins.Keyboard.hide();return true")
        // Keep the socket active beyond the former 40 second resource timeout.
        _ = try await js("window.__qaDisconnects=0;window.__qaWatch=window.twr.onEvent(e=>{if(e.type==='connection'&&!e.connected)window.__qaDisconnects++});return true")
        try await Task.sleep(nanoseconds: 45_000_000_000)
        let drops = try await js("window.__qaWatch();return window.__qaDisconnects") as? Int
        XCTAssertEqual(drops, 0, "Long-lived native WebSocket must remain connected")
        print("LIVENESS_PASS 45-second native socket")
        // Drive actual simulator hinge events, then inspect the app's native region bridge.
        if fixture["foldable"] as? Bool == true {
            try await dismissKeyboard()
            _ = try await js("""
              [...document.querySelectorAll('.mobile-tabs button,.primary-nav button')]
               .find(e=>e.getClientRects().length&&(e.textContent==='Threads'||e.textContent.includes('Thread grid'))).click();return true;
              """)
            try await until("return !![...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker'))")
            _ = try await js("[...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker')).click();return true")
            try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread'))")
            _ = try await js("[...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread')).click();return true")
            try await until("return !!document.querySelector('.composer textarea')")
            _ = try await js("""
              const input=document.querySelector('.composer textarea');
              Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set.call(input,'FOLD_TRANSITION_DRAFT');
              input.dispatchEvent(new Event('input',{bubbles:true}));return true;
              """)
            let closedWidth = try await js("return innerWidth") as? Int ?? 0
            try await assertContentClearOfOcclusions("cover", requireOcclusions: true)
            try await assertComposerReachable("cover")
            try await capture("cover")
            try await pose("open")
            try await until("return innerWidth !== \(closedWidth)")
            try await assertContentClearOfOcclusions("open")
            try await assertComposerReachable("open")
            try await capture("open")
            try await pose("half-open")
            try await until("return (await window.Capacitor.Plugins.TwrLayout.current()).orientation !== 'none'")
            try await assertContentClearOfOcclusions("half-open")
            try await assertComposerReachable("half-open")
            try await capture("half-open")
            try await until("""
              const summary=document.querySelector('.conversation-menu>summary'),box=summary?.getBoundingClientRect();
              return !!box && box.width>=44 && box.height>=44 && summary.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2));
              """)
            _ = try await js("document.querySelector('.conversation-menu>summary').click();return true")
            try await until("""
              const button=[...document.querySelectorAll('.conversation-menu button')].find(e=>e.textContent.trim()==='New thread'),box=button?.getBoundingClientRect();
              return !!box && !button.disabled && box.width>=44 && box.height>=44 && button.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2));
              """)
            _ = try await js("[...document.querySelectorAll('.conversation-menu button')].find(e=>e.textContent.trim()==='New thread').click();return true")
            try await until("return !!document.querySelector('[role=dialog]')")
            let safeDialog = try await js("""
              const box=document.querySelector('[role=dialog]').getBoundingClientRect();
              const style=getComputedStyle(document.documentElement);
              const axis=document.documentElement.dataset.fold;
              return axis==='vertical'
                ? box.left>=parseFloat(style.getPropertyValue('--hinge-left'))+parseFloat(style.getPropertyValue('--hinge-width'))-1
                : box.top>=parseFloat(style.getPropertyValue('--hinge-top'))+parseFloat(style.getPropertyValue('--hinge-height'))-1;
              """) as? Bool
            XCTAssertEqual(safeDialog,true,"Dialog controls must stay clear of the fold")
            _ = try await js("document.querySelector('[aria-label=\"Close dialog\"]').click();return true")
            let draft = try await js("return document.querySelector('.composer textarea').value") as? String
            XCTAssertEqual(draft, "FOLD_TRANSITION_DRAFT")
            try await pose("closed")
            try await until("return innerWidth === \(closedWidth)")
            try await assertContentClearOfOcclusions("reclosed", requireOcclusions: true)
            try await assertComposerReachable("reclosed")
            try await capture("reclosed")
            let restoredDraft = try await js("return document.querySelector('.composer textarea').value") as? String
            XCTAssertEqual(restoredDraft, "FOLD_TRANSITION_DRAFT")
        }
        try await dismissKeyboard()
        let (scriptData, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47423/performance-script")!)
        let performanceResult: Any?
        do {
            performanceResult = try await js(try XCTUnwrap(String(data: scriptData, encoding: .utf8)))
        } catch {
            let diagnostic = try? await js("return {text:document.body.innerText,viewport:{width:innerWidth,height:innerHeight},layout:await window.Capacitor.Plugins.TwrLayout.current()}")
            print("PERFORMANCE FAILURE", String(describing: diagnostic))
            try? await capture("performance-failure")
            throw error
        }
        let performanceData = try JSONSerialization.data(withJSONObject: try XCTUnwrap(performanceResult))
        let performanceAttachment = XCTAttachment(data: performanceData, uniformTypeIdentifier: "public.json")
        performanceAttachment.name = "twr-performance";performanceAttachment.lifetime = .keepAlways;add(performanceAttachment)
        print("PERFORMANCE_JSON " + (String(data: performanceData, encoding: .utf8) ?? "{}"))
        var performanceRequest = URLRequest(url: URL(string: "http://127.0.0.1:47423/performance")!, timeoutInterval: 15)
        performanceRequest.httpMethod = "POST"
        performanceRequest.httpBody = performanceData
        performanceRequest.setValue("close", forHTTPHeaderField: "Connection")
        do { _ = try await URLSession.shared.data(for: performanceRequest) }
        catch { print("Performance JSON export failed; evidence remains in XCTest attachment: \(error)") }
        try await dismissKeyboard()
        try await until("""
          const bar=document.querySelector('.topbar').getBoundingClientRect(),hub=document.querySelector('.mobile-hub-switch').getBoundingClientRect();
          const top=parseFloat(getComputedStyle(document.body).paddingTop)||0;
          return bar.top>=top-1 && hub.top>=bar.top-1 && hub.bottom<=bar.bottom+1 && hub.left>=bar.left && hub.right<=bar.right+1 && hub.width>=44 && hub.height>=44;
          """)
        // Verify landscape safe areas through UIKit, rather than changing CSS dimensions.
        if fixture["foldable"] as? Bool != true && UIDevice.current.userInterfaceIdiom == .phone,
           let scene = web.window?.windowScene {
            let orientations: [UIInterfaceOrientationMask] = [.landscapeRight, .landscapeLeft, .portrait]
            for orientation in orientations {
                scene.requestGeometryUpdate(UIWindowScene.GeometryPreferences.iOS(interfaceOrientations: orientation)) { error in
                    XCTFail("Orientation change failed: \(error)")
                }
                let expected: UIInterfaceOrientation = orientation == .portrait ? .portrait : (orientation == .landscapeRight ? .landscapeRight : .landscapeLeft)
                for _ in 0..<100 {
                    if scene.interfaceOrientation == expected { break }
                    try await Task.sleep(nanoseconds: 100_000_000)
                }
                XCTAssertEqual(scene.interfaceOrientation, expected)
                try await until(orientation == .portrait ? "return innerHeight > innerWidth" : "return innerWidth > innerHeight")
                try await until("""
                  const style=getComputedStyle(document.body),box=document.querySelector('.mobile-hub-switch').getBoundingClientRect();
                  return box.left >= (parseFloat(style.paddingLeft)||0) && box.right <= innerWidth-(parseFloat(style.paddingRight)||0)+1;
                  """)
            }
        }
        // Native file inputs must not steal taps from the adjacent Send control.
        // Check the full viewport first so a real input-overlay issue cannot be
        // mistaken for a side effect of the compact-height contract below.
        try await dismissKeyboard()
        _ = try await js("""
          const input=document.querySelector('.composer textarea');
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set.call(input,'COMPACT_VIEWPORT_SEND_QA');
          input.dispatchEvent(new Event('input',{bubbles:true}));return true;
          """)
        try await until("""
          const upload=document.querySelector('.attachment-button'),send=document.querySelector('button[aria-label="Send message"]');
          return !send.disabled && [upload,send].every(element=>{const box=element.getBoundingClientRect();return box.width>=44 && box.height>=44 && element.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2))});
          """)
        let placeholderFits = try await js("""
          const input=document.querySelector('.composer textarea'),style=getComputedStyle(input),context=document.createElement('canvas').getContext('2d');
          context.font=style.fontWeight+' '+style.fontSize+' '+style.fontFamily;
          return input.placeholder==='Message…' && context.measureText(input.placeholder).width<=input.clientWidth-parseFloat(style.paddingLeft)-parseFloat(style.paddingRight);
          """) as? Bool
        XCTAssertEqual(placeholderFits, true, "The mobile placeholder must fit on one line; the destination chip and accessible label identify the recipient")
        print("PASS: adjacent Upload and Send have separate reachable 44-point targets in the full viewport")
        // Exercise the same available-height contract as keyboard resizing in native WKWebView.
        _ = try await js("""
          const root=document.documentElement,style=getComputedStyle(document.body);
          window.__qaAvailableHeight=root.style.getPropertyValue('--viewport-height');
          window.__qaCompactHeight=Math.max(320,(parseFloat(root.style.getPropertyValue('--occlusion-top'))||0)+230+(parseFloat(style.paddingBottom)||0));
          root.style.setProperty('--viewport-height',window.__qaCompactHeight+'px');
          const input=document.querySelector('.composer textarea');
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set.call(input,'COMPACT_VIEWPORT_SEND_QA');
          input.dispatchEvent(new Event('input',{bubbles:true}));
          window.__qaCompactProbe=(verbose=false)=>{
            const button=document.querySelector('button[aria-label="Send message"]'),box=button.getBoundingClientRect();
            const root=document.getElementById('root'),rootBox=root.getBoundingClientRect();
            const tabs=document.querySelector('.mobile-tabs'),tabsVisible=!!tabs?.getClientRects().length;
            const limit=tabsVisible?tabs.getBoundingClientRect().top:window.__qaCompactHeight;
            const point={x:box.x+box.width/2,y:box.y+box.height/2},hit=document.elementFromPoint(point.x,point.y);
            const upload=document.querySelector('.attachment-button'),uploadBox=upload.getBoundingClientRect(),uploadPoint={x:uploadBox.x+uploadBox.width/2,y:uploadBox.y+uploadBox.height/2};
            const checks={rootWithinAvailableHeight:rootBox.height<=window.__qaCompactHeight+1,enabled:!button.disabled,width44:box.width>=44,height44:box.height>=44,aboveNavigation:box.bottom<=limit+1,ownsHit:button.contains(hit),uploadWidth44:uploadBox.width>=44,uploadHeight44:uploadBox.height>=44,uploadOwnsHit:upload.contains(document.elementFromPoint(uploadPoint.x,uploadPoint.y))};
            const describe=element=>element?{tag:element.tagName,id:element.id,classes:typeof element.className==='string'?element.className:element.className?.baseVal,aria:element.getAttribute('aria-label'),role:element.getAttribute('role')}:null;
            const result={pass:Object.values(checks).every(Boolean),checks,disabled:button.disabled,inputValue:document.querySelector('.composer textarea').value,classes:document.documentElement.className,height:window.__qaCompactHeight,root:rootBox.toJSON(),upload:uploadBox.toJSON(),send:box.toJSON(),tabs:tabs?.getBoundingClientRect().toJSON(),tabsVisible,limit,point,hit:describe(hit)};
            if(!verbose)return result;
            const inspect=element=>{
              if(!element)return null;
              const style=getComputedStyle(element);
              return {...describe(element),bounds:element.getBoundingClientRect().toJSON(),style:{position:style.position,zIndex:style.zIndex,pointerEvents:style.pointerEvents,display:style.display,visibility:style.visibility,opacity:style.opacity,overflow:style.overflow,overflowX:style.overflowX,overflowY:style.overflowY,transform:style.transform,contain:style.contain,isolation:style.isolation,clipPath:style.clipPath},scroll:{left:element.scrollLeft,top:element.scrollTop,width:element.scrollWidth,height:element.scrollHeight,clientWidth:element.clientWidth,clientHeight:element.clientHeight}};
            };
            const viewport=window.visualViewport;
            const offsetPoints=[{name:'center',...point},{name:'plusVisualOffset',x:point.x+(viewport?.offsetLeft||0),y:point.y+(viewport?.offsetTop||0)},{name:'minusVisualOffset',x:point.x-(viewport?.offsetLeft||0),y:point.y-(viewport?.offsetTop||0)},{name:'plusPageScroll',x:point.x+scrollX,y:point.y+scrollY},{name:'minusPageScroll',x:point.x-scrollX,y:point.y-scrollY}];
            const ancestors=[];for(let element=button;element;element=element.parentElement)ancestors.push(inspect(element));
            return {...result,active:describe(document.activeElement),bodyPadding:getComputedStyle(document.body).padding,viewportHeightVariable:document.documentElement.style.getPropertyValue('--viewport-height'),keyboard:document.documentElement.dataset.keyboard,visualViewport:viewport?{width:viewport.width,height:viewport.height,offsetTop:viewport.offsetTop,offsetLeft:viewport.offsetLeft,pageTop:viewport.pageTop,pageLeft:viewport.pageLeft,scale:viewport.scale}:null,window:{width:innerWidth,height:innerHeight,scrollX,scrollY},scrollingElement:inspect(document.scrollingElement),hitStack:document.elementsFromPoint(point.x,point.y).map(inspect),offsetHits:offsetPoints.map(p=>({...p,hit:describe(document.elementFromPoint(p.x,p.y))})),ancestors,surfaces:['.composer','.composer-label','.composer-bottom','.conversation-scroll','.transcript-virtual','.transcript-block','.work-area','.main-shell','.app','#root','body','html'].map(selector=>({selector,...inspect(document.querySelector(selector))}))};
          };return true;
          """)
        print("COMPACT start " + String(describing: try? await js("return JSON.stringify(window.__qaCompactProbe(true))")))
        do {
            try await until("return window.__qaCompactProbe().pass", reportFailure: false)
        } catch {
            print("COMPACT failure " + String(describing: try? await js("return JSON.stringify(window.__qaCompactProbe(true))")))
            print("COMPACT native bounds=\(web.bounds) frame=\(web.frame) offset=\(web.scrollView.contentOffset) contentSize=\(web.scrollView.contentSize) inset=\(web.scrollView.contentInset) adjustedInset=\(web.scrollView.adjustedContentInset) zoomScale=\(web.scrollView.zoomScale)")
            try? await capture("compact-failure")
            XCTFail("Compact composer is not reachable: \(error.localizedDescription)")
            throw error
        }
        print("COMPACT pass " + String(describing: try? await js("return JSON.stringify(window.__qaCompactProbe(true))")))
        try await capture("compact")
        _ = try await js("document.querySelector('button[aria-label=\"Send message\"]').click();return true")
        try await until("return [...document.querySelectorAll('.block-assistant .markdown')].some(e=>e.textContent.trim()==='COMPACT_VIEWPORT_SEND_QA')")
        _ = try await js("document.documentElement.style.setProperty('--viewport-height',window.__qaAvailableHeight);window.dispatchEvent(new Event('resize'));await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);return true")
        try await until("return document.querySelector('button[aria-label^=\"Remote hub settings:\"]').getClientRects().length>0")
        try await verifyLocalNotificationDelivery()
        let layout = try await js("return await window.Capacitor.Plugins.TwrLayout.current()")
        XCTAssertNotNil(layout)
        let result = try await js("return {errors:window.__qaErrors,width:innerWidth,height:innerHeight,layout:await window.Capacitor.Plugins.TwrLayout.current(),identity:(await window.twr.rpc('hub.info')).identity}")
        let attachment = XCTAttachment(string: String(describing: result))
        attachment.name = "Native iOS transport and layout";attachment.lifetime = .keepAlways;add(attachment)
        let errorCount = try await js("return window.__qaErrors.length") as? Int
        XCTAssertEqual(errorCount, 0)
        XCTAssertFalse(repo.isEmpty)
        try await assertContentClearOfOcclusions("final-full-viewport", requireOcclusions: fixture["foldable"] as? Bool == true)
        try await assertComposerReachable("final-full-viewport")
        // Capture the app while it is visible, before XCTest returns to SpringBoard.
        try await capture("screen")
    }
}
