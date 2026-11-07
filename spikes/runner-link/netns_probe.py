"""Two NAT routers and a private relay, in a privileged fixture for trusted code.

The documented invocation uses no host networking/mounts or public relay. Setup
still needs SYS_ADMIN and is not a sandbox; workers drop all capabilities.
"""

import json
import os
from pathlib import Path
import subprocess


def command(*args, check=True):
    return subprocess.run(args, check=check, capture_output=True, text=True, timeout=15)


def in_ns(namespace, *args, check=True):
    return command("ip", "netns", "exec", namespace, *args, check=check)


def fixture(role, namespace=None):
    dropped = ["setpriv", "--reuid=65534", "--regid=65534", "--clear-groups",
               "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all",
               "--no-new-privs", "/probe/nat_fixture", role]
    return ["ip", "netns", "exec", namespace, *dropped] if namespace else dropped


def setup(index):
    router, peer = f"router{index}", f"peer{index}"
    subnet = 66 + index
    for namespace in (router, peer):
        command("ip", "netns", "add", namespace)
        in_ns(namespace, "ip", "link", "set", "lo", "up")
    command("ip", "link", "add", f"wan{index}", "type", "veth", "peer", "name", "wan", "netns", router)
    command("ip", "link", "set", f"wan{index}", "master", "wanbr")
    command("ip", "link", "set", f"wan{index}", "up")
    in_ns(router, "ip", "addr", "add", f"10.66.0.{index+1}/24", "dev", "wan")
    in_ns(router, "ip", "link", "set", "wan", "up")
    in_ns(router, "ip", "link", "add", "lan", "type", "veth", "peer", "name", "eth0", "netns", peer)
    in_ns(router, "ip", "addr", "add", f"10.{subnet}.0.1/24", "dev", "lan")
    in_ns(router, "ip", "link", "set", "lan", "up")
    in_ns(peer, "ip", "addr", "add", f"10.{subnet}.0.2/24", "dev", "eth0")
    in_ns(peer, "ip", "link", "set", "eth0", "up")
    in_ns(peer, "ip", "route", "add", "default", "via", f"10.{subnet}.0.1")
    in_ns(router, "sysctl", "-q", "-w", "net.ipv4.ip_forward=1")
    in_ns(router, "iptables", "-P", "FORWARD", "DROP")
    in_ns(router, "iptables", "-A", "FORWARD", "-i", "lan", "-o", "wan", "-j", "ACCEPT")
    in_ns(router, "iptables", "-A", "FORWARD", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT")
    in_ns(router, "iptables", "-t", "nat", "-A", "POSTROUTING", "-o", "wan", "-j", "MASQUERADE")


def main():
    if os.geteuid() != 0 or not Path("/.dockerenv").exists():
        raise SystemExit("run in the disposable network-disabled Docker fixture, not on the host")
    if os.environ.get("TWR_LINK_NETNS_FIXTURE") != "1":
        raise SystemExit("fixture opt-in is required")
    links = json.loads(command("ip", "-j", "link", "show").stdout)
    if any(link["ifname"] != "lo" for link in links):
        raise SystemExit("fixture requires --network none; refusing setup")
    # Keep kernel sysctls read-only. Temporarily expose only /proc/sys/net for
    # the forwarding writes in our new namespaces, then restore read-only access.
    command("mount", "--bind", "/proc/sys/net", "/proc/sys/net")
    command("mount", "-o", "remount,bind,rw", "/proc/sys/net")
    processes = []
    try:
        command("ip", "link", "add", "wanbr", "type", "bridge")
        command("ip", "addr", "add", "10.66.0.1/24", "dev", "wanbr")
        command("ip", "link", "set", "wanbr", "up")
        try:
            setup(1)
            setup(2)
        finally:
            command("mount", "-o", "remount,bind,ro", "/proc/sys/net")
        # A private runner LAN address is not routable from the hub LAN.
        assert in_ns("peer1", "ping", "-c", "1", "-W", "1", "10.68.0.2", check=False).returncode != 0
        relay = subprocess.Popen(fixture("relay"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        processes.append(relay)
        server = subprocess.Popen(fixture("server", "peer2"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        processes.append(server)
        client = subprocess.run(fixture("client", "peer1"), capture_output=True, text=True, timeout=100)
        if client.returncode:
            raise RuntimeError("NAT client failed: " + client.stderr)
        _, errors = server.communicate(timeout=10)
        if server.returncode:
            raise RuntimeError("NAT server failed: " + errors)
        result = json.loads(client.stdout)
        assert result["two_nat_transfer"] and not result["public_infrastructure"]
        assert result["direct"] is False, "fixture must exercise relay fallback"
        print(json.dumps(result))
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()
        for namespace in ("peer1", "peer2", "router1", "router2"):
            command("ip", "netns", "delete", namespace, check=False)
        command("ip", "link", "delete", "wanbr", check=False)


if __name__ == "__main__":
    main()
