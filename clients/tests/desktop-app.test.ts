import { describe, expect, it } from "vitest";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
  mkdirSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
  AUTOSTART_ARG,
  autostartEntry,
  autostartPath,
  desktopExecArgument,
  linuxAutostartEnabled,
  linuxLaunchCommand,
  setLinuxAutostart,
  refreshLinuxAutostart,
} from "../desktop/autostart";
import {
  compareVersions,
  detectInstallKind,
  developerIdSigned,
  installerFor,
  manualInstructions,
  updateMetadataName,
} from "../desktop/update-policy";
import {
  DiagnosticLog,
  RateLimiter,
  countCrashReports,
  diagnosticsReport,
  redact,
} from "../desktop/diagnostics";

describe("launch at login on Linux", () => {
  it("follows XDG_CONFIG_HOME", () => {
    expect(autostartPath({ XDG_CONFIG_HOME: "/x" }, "/home/u")).toBe(
      "/x/autostart/tools.twr.desktop.desktop",
    );
    expect(autostartPath({}, "/home/u")).toBe(
      "/home/u/.config/autostart/tools.twr.desktop.desktop",
    );
  });
  it("starts AppImages through the image and packages through their launcher", () => {
    expect(
      linuxLaunchCommand({ APPIMAGE: "/a/twr.AppImage" }, "/tmp/.mount/twr"),
    ).toBe("/a/twr.AppImage");
    expect(
      linuxLaunchCommand(
        {},
        "/opt/twr/twr-desktop",
        (p) => p === "/usr/bin/twr-desktop",
      ),
    ).toBe("/usr/bin/twr-desktop");
    expect(linuxLaunchCommand({}, "/x/twr-desktop", () => false)).toBe(
      "/x/twr-desktop",
    );
  });
  it("quotes Exec arguments per the Desktop Entry specification", () => {
    expect(desktopExecArgument("/opt/twr app/twr")).toBe('"/opt/twr app/twr"');
    expect(desktopExecArgument('/a/"$x`\\y%')).toBe(
      '"/a/\\\\"\\\\$x\\\\`\\\\\\\\y%%"',
    );
    expect(() => desktopExecArgument("/a\nb")).toThrow();
    expect(autostartEntry("/usr/bin/twr-desktop")).toContain(
      `Exec="/usr/bin/twr-desktop" ${AUTOSTART_ARG}\n`,
    );
  });
  it("writes, detects and removes only its own entry", () => {
    const dir = mkdtempSync(join(tmpdir(), "twr-autostart-"));
    const file = join(dir, "autostart", "tools.twr.desktop.desktop");
    expect(linuxAutostartEnabled(file)).toBe(false);
    setLinuxAutostart(file, true, "/usr/bin/twr-desktop");
    expect(linuxAutostartEnabled(file)).toBe(true);
    setLinuxAutostart(file, false, "/usr/bin/twr-desktop");
    expect(existsSync(file)).toBe(false);
    // A user's own entry with the same name is never deleted.
    mkdirSync(join(dir, "autostart"), { recursive: true });
    writeFileSync(file, "[Desktop Entry]\nExec=custom\n");
    expect(linuxAutostartEnabled(file)).toBe(false);
    expect(() => setLinuxAutostart(file, false, "x")).toThrow(
      /not created by twr/,
    );
    expect(readFileSync(file, "utf8")).toContain("custom");
    // A session that disabled the entry is reported as off.
    writeFileSync(file, autostartEntry("x") + "Hidden=true\n");
    expect(linuxAutostartEnabled(file)).toBe(false);
  });
});

describe("update policy", () => {
  const base = { packaged: true, env: {}, execPath: "/x/twr-desktop" };
  it("detects how this copy was installed", () => {
    expect(detectInstallKind({ ...base, platform: "darwin" })).toBe("mac");
    expect(
      detectInstallKind({
        ...base,
        platform: "linux",
        env: { APPIMAGE: "/a" },
      }),
    ).toBe("appimage");
    expect(
      detectInstallKind({ ...base, platform: "linux", packageType: "deb" }),
    ).toBe("deb");
    expect(
      detectInstallKind({
        ...base,
        platform: "linux",
        execPath: "/opt/twr-desktop/twr-desktop",
      }),
    ).toBe("pacman");
    expect(detectInstallKind({ ...base, platform: "linux" })).toBe("portable");
    expect(
      detectInstallKind({ ...base, platform: "darwin", packaged: false }),
    ).toBe("development");
  });
  it("requires a Developer ID signature for macOS self-updates", () => {
    expect(
      developerIdSigned(
        "Authority=Developer ID Application: Example (ABCDE12345)\nTeamIdentifier=ABCDE12345\n",
      ),
    ).toBe(true);
    expect(developerIdSigned("Signature=adhoc\nTeamIdentifier=not set\n")).toBe(
      false,
    );
  });
  it("orders semantic versions including prereleases", () => {
    expect(compareVersions("v0.3.1", "0.3.0")).toBe(1);
    expect(compareVersions("0.3.0", "0.3.0")).toBe(0);
    expect(compareVersions("0.10.0", "0.9.9")).toBe(1);
    expect(compareVersions("1.0.0-rc.1", "1.0.0")).toBe(-1);
    expect(compareVersions("1.0.0-rc.2", "1.0.0-rc.10")).toBe(-1);
    expect(compareVersions("1.0.0-alpha", "1.0.0-1")).toBe(1);
    expect(() => compareVersions("latest", "1.0.0")).toThrow();
  });
  it("picks the installer and instructions for each kind", () => {
    const assets = [
      "twr-0.4.0-arm64.dmg",
      "twr-0.4.0-arm64-mac.zip",
      "twr-0.4.0.AppImage",
      "twr-desktop_0.4.0_amd64.deb",
      "twr-desktop-bin-0.4.0-1-x86_64.pkg.tar.zst",
      "twr-desktop-0.4.0-linux-x64.tar.gz",
      "latest-mac.yml",
    ].map((name) => ({ name, url: `https://example.test/${name}` }));
    expect(installerFor("mac", assets)?.name).toBe("twr-0.4.0-arm64.dmg");
    expect(installerFor("appimage", assets)?.name).toBe("twr-0.4.0.AppImage");
    expect(installerFor("deb", assets)?.name).toBe(
      "twr-desktop_0.4.0_amd64.deb",
    );
    expect(installerFor("pacman", assets)?.name).toMatch(/pkg\.tar\.zst$/);
    expect(installerFor("portable", assets)?.name).toMatch(
      /linux-x64\.tar\.gz$/,
    );
    expect(installerFor("development", assets)).toBeUndefined();
    expect(updateMetadataName("mac")).toBe("latest-mac.yml");
    expect(updateMetadataName("appimage")).toBe("latest-linux.yml");
    expect(updateMetadataName("deb")).toBeUndefined();
    expect(manualInstructions("deb", installerFor("deb", assets))).toContain(
      "sudo apt install ./twr-desktop_0.4.0_amd64.deb",
    );
    expect(manualInstructions("pacman", undefined)).toContain(
      "twr-desktop-bin",
    );
  });
});

describe("diagnostics", () => {
  it("redacts credentials and the home directory", () => {
    const text = [
      "Authorization: Bearer abcdefghijklmnop",
      'config {"api_key": "sk-ant-abcdefghijklmnopqrstuvwxyz"}',
      "token=deadbeefdeadbeef pairing_code=123456",
      "https://user:pass@example.com/x?code=99&ok=1",
      "ghp_abcdefghijklmnopqrstuvwxyz0123",
      "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
      "opened /Users/alex/Code/project",
    ].join("\n");
    const result = redact(text, "/Users/alex");
    for (const secret of [
      "abcdefghijklmnop",
      "sk-ant-",
      "deadbeef",
      "123456",
      "user:pass",
      "code=99",
      "ghp_",
      "BEGIN OPENSSH",
      "/Users/alex",
    ])
      expect(result).not.toContain(secret);
    expect(result).toContain("~/Code/project");
    expect(result).toContain("ok=1");
  });
  it("rotates the log and tails across both files", () => {
    const dir = mkdtempSync(join(tmpdir(), "twr-log-"));
    const log = new DiagnosticLog(dir, "/home/u", 400);
    for (let i = 0; i < 20; i++) log.info("test", `line ${i} /home/u/x`);
    expect(existsSync(join(dir, "twr-desktop.1.log"))).toBe(true);
    const tail = log.tail(5);
    expect(tail.split("\n")).toHaveLength(5);
    expect(tail).toContain("line 19 ~/x");
    log.error("test", "boom", new Error("bad token=abcdefabcdef"));
    expect(log.tail(100)).toContain("bad token=[redacted]");
    expect(log.tail(100)).not.toContain("abcdefabcdef");
  });
  it("bounds renderer error reports", () => {
    let now = 0;
    const limiter = new RateLimiter(2, 1000, () => now);
    expect([limiter.allow(), limiter.allow(), limiter.allow()]).toEqual([
      true,
      true,
      false,
    ]);
    now = 1001;
    expect(limiter.allow()).toBe(true);
  });
  it("counts crash dumps and builds a redacted report", () => {
    const dir = mkdtempSync(join(tmpdir(), "twr-crash-"));
    mkdirSync(join(dir, "completed"), { recursive: true });
    writeFileSync(join(dir, "completed", "a.dmp"), "");
    writeFileSync(join(dir, "settings.dat"), "");
    expect(countCrashReports(dir)).toBe(1);
    expect(countCrashReports(join(dir, "missing"))).toBe(0);
    const report = diagnosticsReport(
      { version: "0.3.0", path: "/home/u/.config", tray: { enabled: true } },
      "log secret=abc123456",
      "/home/u",
    );
    expect(report).toContain("version: 0.3.0");
    expect(report).toContain('tray: {"enabled":true}');
    expect(report).toContain("~/.config");
    expect(report).not.toContain("abc123456");
  });
});

describe("AUR package", () => {
  it("matches the Arch package layout and generates .SRCINFO", async () => {
    // @ts-expect-error untyped build script
    const { aurFiles } = await import("../scripts/aur.mjs");
    const arch = readFileSync("packaging/arch/PKGBUILD.in", "utf8");
    const template = readFileSync("packaging/aur/PKGBUILD.in", "utf8");
    const body = (text: string) => text.slice(text.indexOf("package() {"));
    expect(body(template)).toBe(body(arch));
    const depends = (text: string) => /^depends=.*$/m.exec(text)?.[0];
    expect(depends(template)).toBe(depends(arch));
    const hash = "a".repeat(64);
    const { pkgbuild, srcinfo } = aurFiles(template, "0.4.0", hash);
    expect(pkgbuild).toContain("pkgver=0.4.0");
    expect(pkgbuild).not.toContain("@");
    expect(srcinfo).toContain("pkgbase = twr-desktop-bin\n");
    expect(srcinfo).toContain(
      "\tsource = twr-desktop-bin-0.4.0.tar.gz::https://github.com/ignaval/twr/releases/download/v0.4.0/twr-desktop-0.4.0-linux-x64.tar.gz\n",
    );
    expect(srcinfo).toContain(`\tsha256sums = ${hash}\n`);
    expect(srcinfo).toContain("\tprovides = twr-desktop\n");
    expect(srcinfo).toContain("\toptdepends = openssh: remote boxes\n");
    expect(() => aurFiles(template, "0.4.0-rc.1", hash)).toThrow(/stable/);
  });
});

describe("diagnostics redaction gaps", () => {
  it("covers cookies, fragments, pairing codes and more key shapes", () => {
    const text = [
      "Cookie: a=1; sessionkey=SECRETCOOKIE",
      "https://example.test/cb#access_token=FRAGMENTSECRET&ok=1",
      '{"code":"ABCD-1234","host":"x"}',
      "github_pat_11ABCDEFGHIJKLMNOPQRSTUVWXYZ012345",
      "AKIAABCDEFGHIJKLMNOP",
      "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlX3ZhbHVl",
    ].join("\n");
    const result = redact(text);
    for (const secret of [
      "SECRETCOOKIE",
      "FRAGMENTSECRET",
      "ABCD-1234",
      "github_pat_11",
      "AKIAABCDEFGHIJKLMNOP",
      "eyJhbGci",
    ])
      expect(result).not.toContain(secret);
    expect(result).toContain('"host":"x"');
    expect(redact('{"code":"ENOENT","exit":{"code":1}}')).toBe(
      '{"code":"ENOENT","exit":{"code":1}}',
    );
    // The real pairing format, even when it happens to look like a constant.
    for (const code of ["K7MQ2WXR9P", "EABCDEFGHJ"])
      expect(redact(`{"code":"${code}"}`)).toBe('{"code":"[redacted]"}');
    expect(redact('{"code":"REQUEST_TOO_LARGE"}')).toContain(
      "REQUEST_TOO_LARGE",
    );
  });
  it("keeps a Linux login entry pointing at a renamed AppImage", () => {
    const dir = mkdtempSync(join(tmpdir(), "twr-autostart-refresh-"));
    const file = join(dir, "tools.twr.desktop.desktop");
    expect(refreshLinuxAutostart(file, "/a/twr-0.4.0.AppImage")).toBe(false);
    setLinuxAutostart(file, true, "/a/twr-0.3.0.AppImage");
    expect(refreshLinuxAutostart(file, "/a/twr-0.3.0.AppImage")).toBe(false);
    expect(refreshLinuxAutostart(file, "/a/twr-0.4.0.AppImage")).toBe(true);
    expect(readFileSync(file, "utf8")).toContain(
      'Exec="/a/twr-0.4.0.AppImage"',
    );
  });
});
