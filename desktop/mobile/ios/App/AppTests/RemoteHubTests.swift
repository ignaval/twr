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

    private func until(_ body: String, seconds: Double = 30) async throws {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            let check = """
              return await Promise.race([(async()=>{\(body)})(),new Promise(resolve=>setTimeout(()=>resolve(false),3000))]);
              """
            if (try? await js(check)) as? Bool == true { return }
            try await Task.sleep(nanoseconds: 100_000_000)
        }
        let html = (try? await js("return document.body.innerText")) as? String ?? "No document"
        XCTFail("Timed out: \(body)\n\(html.prefix(3000))")
        throw NSError(domain: "twr.qa", code: 1)
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
        try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent==='Pair a hub')")
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
        try await until("return JSON.stringify(window.__qaEvents).includes('IOS_REMOTE_STREAM_OK')")
        _ = try await js("""
          const visible=e=>e.getClientRects().length>0;
          const nav=[...document.querySelectorAll('.mobile-tabs button,.primary-nav button')]
            .find(e=>visible(e)&&(e.textContent==='Threads'||e.textContent.includes('Thread grid')));nav.click();return true;
          """)
        try await until("return !![...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker'))")
        _ = try await js("[...document.querySelectorAll('.thread-grid .thread-card')].find(e=>e.textContent.includes('ios-worker')).click();return true")
        try await until("return !![...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread'))")
        _ = try await js("[...document.querySelectorAll('button')].find(e=>e.textContent.includes('Talk to this thread')).click();return true")
        try await until("return document.body.innerText.includes('IOS_REMOTE_STREAM_OK') && !!document.querySelector('.composer textarea')")
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
        try await until("return JSON.stringify(window.__qaEvents).includes('IOS_ANSWERED')")
        let commandCount = try await js("return (await window.twr.catalog()).length") as? Int
        XCTAssertEqual(commandCount, 71)
        let sizeGuard = try await js("""
          try { await window.Capacitor.Plugins.TwrTransport.send({message:'界'.repeat(750000)}); return false }
          catch(error) { return error.code==='REQUEST_TOO_LARGE' }
          """) as? Bool
        XCTAssertEqual(sizeGuard, true, "Native requests must be bounded by UTF-8 bytes without closing the socket")
        _ = try await js("""
          window.__qaTerminal=await window.twr.terminalOpen({shell:true,title:'iOS QA'});
          const command="printf 'ORDER_%s\\\\n' '0123456789abcdefghijklmnopqrstuvwxyz'\\r";
          for(const char of command)window.twr.terminalWrite(window.__qaTerminal.id,char);return true;
          """)
        try await until("return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')")
        // Reset the real TCP connection, keeping the gateway and its PTY alive.
        _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/drop-socket")!)
        try await until("try{return (await window.twr.terminalRead(window.__qaTerminal.id)).data.includes('ORDER_0123456789abcdefghijklmnopqrstuvwxyz')}catch{return false}", seconds: 45)
        _ = try await js("await window.twr.terminalClose(window.__qaTerminal.id);return true")
        // Keep the socket active beyond the former 40 second resource timeout.
        _ = try await js("window.__qaDisconnects=0;window.__qaWatch=window.twr.onEvent(e=>{if(e.type==='connection'&&!e.connected)window.__qaDisconnects++});return true")
        try await Task.sleep(nanoseconds: 45_000_000_000)
        let drops = try await js("window.__qaWatch();return window.__qaDisconnects") as? Int
        XCTAssertEqual(drops, 0, "Long-lived native WebSocket must remain connected")
        // Drive actual simulator hinge events, then inspect the app's native region bridge.
        if fixture["foldable"] as? Bool == true {
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
            _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/pose/open")!)
            try await until("return innerWidth !== \(closedWidth)")
            _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/pose/half-open")!)
            try await until("return (await window.Capacitor.Plugins.TwrLayout.current()).orientation !== 'none'")
            _ = try await js("[...document.querySelectorAll('.topbar button')].find(e=>e.getAttribute('aria-label')==='New thread'||e.textContent.includes('New thread')).click();return true")
            try await until("return !!document.querySelector('[role=dialog]')")
            let safeDialog = try await js("""
              const box=document.querySelector('[role=dialog]').getBoundingClientRect();
              const style=getComputedStyle(document.documentElement);
              const axis=document.documentElement.dataset.fold;
              return axis==='vertical'
                ? box.left>=parseFloat(style.getPropertyValue('--fold-left'))+parseFloat(style.getPropertyValue('--hinge-width'))-1
                : box.top>=parseFloat(style.getPropertyValue('--fold-top'))+parseFloat(style.getPropertyValue('--hinge-height'))-1;
              """) as? Bool
            XCTAssertEqual(safeDialog,true,"Dialog controls must stay clear of the fold")
            _ = try await js("document.querySelector('[aria-label=\"Close dialog\"]').click();return true")
            let draft = try await js("return document.querySelector('.composer textarea').value") as? String
            XCTAssertEqual(draft, "FOLD_TRANSITION_DRAFT")
            _ = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/pose/closed")!)
            try await until("return innerWidth === \(closedWidth)")
            let restoredDraft = try await js("return document.querySelector('.composer textarea').value") as? String
            XCTAssertEqual(restoredDraft, "FOLD_TRANSITION_DRAFT")
        }
        let (scriptData, _) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/performance-script")!)
        let performanceResult = try await js(try XCTUnwrap(String(data: scriptData, encoding: .utf8)))
        var performanceRequest = URLRequest(url: URL(string: "http://127.0.0.1:47323/performance")!)
        performanceRequest.httpMethod = "POST"
        performanceRequest.httpBody = try JSONSerialization.data(withJSONObject: try XCTUnwrap(performanceResult))
        _ = try await URLSession.shared.data(for: performanceRequest)
        let layout = try await js("return await window.Capacitor.Plugins.TwrLayout.current()")
        XCTAssertNotNil(layout)
        let result = try await js("return {errors:window.__qaErrors,width:innerWidth,height:innerHeight,layout:await window.Capacitor.Plugins.TwrLayout.current(),identity:(await window.twr.rpc('hub.info')).identity}")
        let attachment = XCTAttachment(string: String(describing: result))
        attachment.name = "Native iOS transport and layout";attachment.lifetime = .keepAlways;add(attachment)
        let errorCount = try await js("return window.__qaErrors.length") as? Int
        XCTAssertEqual(errorCount, 0)
        XCTAssertFalse(repo.isEmpty)
        // Capture the app while it is visible, before XCTest returns to SpringBoard.
        let (_, screenshotResponse) = try await URLSession.shared.data(from: URL(string: "http://127.0.0.1:47323/screenshot")!)
        XCTAssertEqual((screenshotResponse as? HTTPURLResponse)?.statusCode, 200)
    }
}
