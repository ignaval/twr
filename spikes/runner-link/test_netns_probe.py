import os
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import netns_probe


class HarnessGuardTests(unittest.TestCase):
    def test_networked_or_host_network_container_is_refused_before_mutation(self):
        with patch.dict(os.environ, {"TWR_LINK_NETNS_FIXTURE": "1"}), \
             patch("netns_probe.os.geteuid", return_value=0), \
             patch("netns_probe.Path.exists", return_value=True), \
             patch("netns_probe.command", return_value=SimpleNamespace(
                 stdout='[{"ifname":"lo"},{"ifname":"eth0"}]')) as command:
            with self.assertRaisesRegex(SystemExit, "requires --network none"):
                netns_probe.main()
            command.assert_called_once_with("ip", "-j", "link", "show")


if __name__ == "__main__":
    unittest.main()
