import unittest

from aiengine.rules import validate_ruleset


class RulesTest(unittest.TestCase):
    def test_valid_and_normalised(self):
        r = validate_ruleset({"name": "My Rule!", "rules": [{"dest": "Photos/{year}/{attr.event}", "filter": {"mime_prefix": ["image/"]}}]})
        self.assertEqual(r["name"], "My-Rule")
        self.assertEqual(r["attributes"], [{"name": "event", "description": ""}])  # undeclared attr auto-declared
        self.assertEqual(r["rules"][0]["filter"], {"mime_prefix": ["image/"]})

    def test_rejects_escapes(self):
        for dest in ["../x", "/abs", "a/../b", "{nope}", "{year", "a}b", "{attr.}"]:
            with self.assertRaises(ValueError, msg=dest):
                validate_ruleset({"rules": [{"dest": dest}]})

    def test_rejects_bad_shapes(self):
        bad = [
            [], {}, {"rules": []}, {"rules": ["x"]}, {"rules": [{"dest": ""}]},
            {"rules": [{"dest": "a", "filter": {"ext": "rs"}}]},
            {"rules": [{"dest": "a", "filter": {"min_size": -1}}]},
            {"rules": [{"dest": "a", "filter": {"min_size": True}}]},
            {"rules": [{"dest": "a", "filter": {"attr_equals": {"k": 1}}}]},
            {"attributes": [{"name": "1bad"}], "rules": [{"dest": "a"}]},
            {"rules": [{"dest": "a"}] * 21},
        ]
        for b in bad:
            with self.assertRaises(ValueError, msg=str(b)):
                validate_ruleset(b)

    def test_attr_equals_declares_attribute(self):
        r = validate_ruleset({"rules": [{"dest": "S", "filter": {"attr_equals": {"kind": "source"}}}]})
        self.assertEqual([a["name"] for a in r["attributes"]], ["kind"])

    def test_drops_unknown_keys(self):
        r = validate_ruleset({"rules": [{"dest": "a", "evil": "x", "filter": {"weird": 1}}], "extra": 1})
        self.assertNotIn("evil", r["rules"][0])
        self.assertEqual(r["rules"][0]["filter"], {})


if __name__ == "__main__":
    unittest.main()
