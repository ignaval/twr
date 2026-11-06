import XCTest
import WebKit
import UIKit
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

    private func statusBar(_ expected: UIStatusBarStyle) async throws {
        let controller = try XCTUnwrap(web.window?.rootViewController as? TwrViewController)
        let backgroundValue = try await js("return getComputedStyle(document.documentElement).getPropertyValue('--bg').trim().slice(1)") as? String
        let background = try XCTUnwrap(backgroundValue)
        let rgb = try XCTUnwrap(UInt32(background, radix: 16))
        let expectedColor = UIColor(red: CGFloat((rgb >> 16) & 255) / 255,
                                    green: CGFloat((rgb >> 8) & 255) / 255,
                                    blue: CGFloat(rgb & 255) / 255, alpha: 1)
        for _ in 0..<100 {
            if controller.preferredStatusBarStyle == expected && controller.view.backgroundColor == expectedColor && web.backgroundColor == expectedColor { return }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertEqual(controller.preferredStatusBarStyle, expected, "Native status-bar contrast must match the rendered theme")
        XCTAssertEqual(controller.view.backgroundColor, expectedColor, "Native background must match the rendered theme")
        XCTAssertEqual(web.backgroundColor, expectedColor, "WebView background must match the rendered theme")
    }

    private func capture(_ name: String) async throws {
        let image: UIImage = try await withCheckedThrowingContinuation { continuation in
            web.takeSnapshot(with: nil) { image, error in
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
        let bounds = try? await js("""
          const box=selector=>document.querySelector(selector)?.getBoundingClientRect().toJSON();
          return JSON.stringify({name,viewport:{width:innerWidth,height:innerHeight,visualHeight:visualViewport?.height},root:box('#root'),send:box('button[aria-label="Send message"]'),tabs:box('.mobile-tabs'),bodyPadding:getComputedStyle(document.body).padding,keyboard:document.documentElement.dataset.keyboard,fold:document.documentElement.dataset.fold});
          """, ["name":name])
        print("UI_BOUNDS " + String(describing: bounds))
        var request = URLRequest(url: URL(string: "http://127.0.0.1:47323/screenshot?name=" + name)!, timeoutInterval: 15)
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
        let request=URLRequest(url:URL(string:"http://127.0.0.1:47323/pose/"+name)!,timeoutInterval:240)
        let (data,response)=try await URLSession.shared.data(for:request)
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode,200,String(data:data,encoding:.utf8) ?? "Pose failed")
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
        let (data, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/fixture")!)
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let code = try XCTUnwrap(fixture["code"] as? String)
        let hubURL = try XCTUnwrap(fixture["url"] as? String)
        let repo = try XCTUnwrap(fixture["repo"] as? String)
        if fixture["foldable"] as? Bool == true {try await pose("closed")}

        try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent==='Pair a hub')")
        // Verify native contrast independently of the OS's current appearance.
        let controller = try XCTUnwrap(web.window?.rootViewController as? TwrViewController)
        controller.overrideUserInterfaceStyle = .light
        try await until("return document.documentElement.dataset.theme === 'dark'")
        try await statusBar(.lightContent)
        _ = try await js("document.documentElement.dataset.theme='light';return true")
        try await statusBar(.darkContent)
        _ = try await js("document.documentElement.dataset.theme='system';return true")
        try await until("return getComputedStyle(document.documentElement).colorScheme === 'light'")
        try await statusBar(.darkContent)
        controller.overrideUserInterfaceStyle = .dark
        try await until("return getComputedStyle(document.documentElement).colorScheme === 'dark'")
        try await statusBar(.lightContent)
        controller.overrideUserInterfaceStyle = .unspecified
        _ = try await js("document.documentElement.dataset.theme='dark';return true")
        try await statusBar(.lightContent)
        print("PASS: native status-bar contrast follows Dark, Light, System and native trait changes")
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
        let (verification, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/verify-upload")!)
        XCTAssertEqual(String(data: verification, encoding: .utf8), "IOS_UPLOAD_OK")
        _ = try await js("""
          await window.twr.rpc('thread.send',{thread:'ios-worker',prompt:'!ask Continue iOS QA?|Proceed|Wait\\nIOS_ANSWERED'});return true;
          """)
        try await until("return JSON.stringify(window.__qaEvents).includes('Continue iOS QA?')")
        _ = try await js("""
          [...document.querySelectorAll('.mobile-tabs button,.primary-nav button')]
           .find(e=>e.getClientRects().length&&e.textContent.includes('Needs you')).click();return true;
          """)
        try await until("return !![...document.querySelectorAll('.attention-card button')].find(e=>e.textContent.includes('Proceed'))")
        _ = try await js("[...document.querySelectorAll('.attention-card button')].find(e=>e.textContent.includes('Proceed')).click();return true")
        try await until("return window.__qaEvents.some(e=>e.type==='thread' && e.event?.kind==='agent.text' && e.event?.data?.text?.includes('IOS_ANSWERED'))")
        let commandCount = try await js("return (await window.twr.catalog()).length") as? Int
        XCTAssertEqual(commandCount, 71)
        let sizeGuard = try await js("""
          try { await window.Capacitor.Plugins.TwrTransport.send({message:'界'.repeat(750000)}); return false }
          catch(error) { return error.code==='REQUEST_TOO_LARGE' }
          """) as? Bool
        XCTAssertEqual(sizeGuard, true, "Native requests must be bounded by UTF-8 bytes without closing the socket")
        _ = try await js("document.querySelector('button[aria-label=\"Open shell\"]').click();return true")
        try await until("return !!document.querySelector('.terminal-session .xterm-helper-textarea') && !!document.querySelector('.terminal-tab.active button[aria-label^=\"Close \"]')")
        _ = try await js("""
          window.__qaTerminal={id:document.querySelector('.terminal-session').dataset.sessionId};
          const command="printf 'ORDER_%s\\\\n' '0123456789abcdefghijklmnopqrstuvwxyz'\\r";
          for(const char of command)window.twr.terminalWrite(window.__qaTerminal.id,char);return true;
          """)
        try await until("return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')")
        // Reset the real TCP connection, keeping the gateway and its PTY alive.
        _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/drop-socket")!)
        try await until("try{return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')}catch{return false}", seconds: 45)
        // Close through the app so both the remote PTY and its local pane disappear.
        // Calling the host API alone intentionally leaves the terminal's UI history open.
        _ = try await js("document.querySelector('.terminal-tab.active button[aria-label^=\"Close \"]').click();return true")
        try await until("return !!document.querySelector('[role=dialog]')")
        _ = try await js("[...document.querySelectorAll('[role=dialog] button')].find(e=>e.textContent==='Continue').click();return true")
        try await until("return !document.querySelector('.terminal-dock')")
        _ = try await js("await window.Capacitor.Plugins.Keyboard.hide();return true")
        // Keep the socket active beyond the former 40 second resource timeout.
        _ = try await js("window.__qaDisconnects=0;window.__qaWatch=window.twr.onEvent(e=>{if(e.type==='connection'&&!e.connected)window.__qaDisconnects++});return true")
        try await Task.sleep(nanoseconds: 45_000_000_000)
        let drops = try await js("window.__qaWatch();return window.__qaDisconnects") as? Int
        XCTAssertEqual(drops, 0, "Long-lived native WebSocket must remain connected")
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
            try await capture("cover")
            try await pose("open")
            try await until("return innerWidth !== \(closedWidth)")
            try await capture("open")
            try await pose("half-open")
            try await until("return (await window.Capacitor.Plugins.TwrLayout.current()).orientation !== 'none'")
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
            let restoredDraft = try await js("return document.querySelector('.composer textarea').value") as? String
            XCTAssertEqual(restoredDraft, "FOLD_TRANSITION_DRAFT")
        }
        try await dismissKeyboard()
        let (scriptData, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/performance-script")!)
        let performanceResult: Any?
        do {
            performanceResult = try await js(try XCTUnwrap(String(data: scriptData, encoding: .utf8)))
        } catch {
            let diagnostic = try? await js("return {text:document.body.innerText,viewport:{width:innerWidth,height:innerHeight},layout:await window.Capacitor.Plugins.TwrLayout.current()}")
            print("PERFORMANCE FAILURE", String(describing: diagnostic))
            try? await capture("performance-failure")
            throw error
        }
        var performanceRequest = URLRequest(url: URL(string: "http://127.0.0.1:47323/performance")!)
        performanceRequest.httpMethod = "POST"
        performanceRequest.httpBody = try JSONSerialization.data(withJSONObject: try XCTUnwrap(performanceResult))
        _ = try await URLSession.shared.data(for: performanceRequest)
        try await until("const top=parseFloat(getComputedStyle(document.body).paddingTop)||0;return Math.abs(document.querySelector('.mobile-hub-switch').getBoundingClientRect().top-top-8)<=1")
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
        // Exercise the same available-height contract as keyboard resizing in native WKWebView.
        try await dismissKeyboard()
        _ = try await js("""
          const root=document.documentElement,style=getComputedStyle(document.body);
          window.__qaAvailableHeight=root.style.getPropertyValue('--viewport-height');
          window.__qaCompactHeight=Math.max(320,(parseFloat(root.style.getPropertyValue('--occlusion-top'))||0)+230+(parseFloat(style.paddingBottom)||0));
          root.style.setProperty('--viewport-height',window.__qaCompactHeight+'px');
          const input=document.querySelector('.composer textarea');
          Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set.call(input,'COMPACT_VIEWPORT_SEND_QA');
          input.dispatchEvent(new Event('input',{bubbles:true}));return true;
          """)
        do {
        try await until("""
          const button=document.querySelector('button[aria-label="Send message"]'),box=button.getBoundingClientRect();
          const tabs=document.querySelector('.mobile-tabs'),limit=tabs?.getClientRects().length?tabs.getBoundingClientRect().top:window.__qaCompactHeight;
          return document.getElementById('root').getBoundingClientRect().height<=window.__qaCompactHeight+1 && !button.disabled && box.width>=44 && box.height>=44 && box.bottom<=limit+1 && button.contains(document.elementFromPoint(box.x+box.width/2,box.y+box.height/2));
          """, reportFailure: false)
        } catch {
            print("COMPACT " + String(describing: try? await js("return {classes:document.documentElement.className,height:window.__qaCompactHeight,root:document.getElementById('root').getBoundingClientRect().toJSON(),send:document.querySelector('button[aria-label=\"Send message\"]').getBoundingClientRect().toJSON(),tabs:document.querySelector('.mobile-tabs')?.getBoundingClientRect().toJSON(),body:getComputedStyle(document.body).padding,active:document.activeElement?.tagName}")))
            try? await capture("compact-failure")
            XCTFail("Compact composer is not reachable: \(error.localizedDescription)")
            throw error
        }
        try await capture("compact")
        _ = try await js("document.querySelector('button[aria-label=\"Send message\"]').click();return true")
        try await until("return [...document.querySelectorAll('.block-assistant .markdown')].some(e=>e.textContent.trim()==='COMPACT_VIEWPORT_SEND_QA')")
        _ = try await js("document.documentElement.style.setProperty('--viewport-height',window.__qaAvailableHeight);window.dispatchEvent(new Event('resize'));await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);return true")
        let layout = try await js("return await window.Capacitor.Plugins.TwrLayout.current()")
        XCTAssertNotNil(layout)
        let result = try await js("return {errors:window.__qaErrors,width:innerWidth,height:innerHeight,layout:await window.Capacitor.Plugins.TwrLayout.current(),identity:(await window.twr.rpc('hub.info')).identity}")
        let attachment = XCTAttachment(string: String(describing: result))
        attachment.name = "Native iOS transport and layout";attachment.lifetime = .keepAlways;add(attachment)
        let errorCount = try await js("return window.__qaErrors.length") as? Int
        XCTAssertEqual(errorCount, 0)
        XCTAssertFalse(repo.isEmpty)
        // Capture the app while it is visible, before XCTest returns to SpringBoard.
        try await capture("screen")
    }
}
