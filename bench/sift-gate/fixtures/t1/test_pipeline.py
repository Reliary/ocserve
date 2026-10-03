import unittest
from parser import parse_config, validate_config
from validator import validate_user
from formatter import render_table

class TestPipeline(unittest.TestCase):

    def test_parse_00(self):
        cfg = parse_config(["k0 = v0", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k0"], "v0")
        self.assertNotIn("junk line", cfg)

    def test_parse_01(self):
        cfg = parse_config(["k1 = v1", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k1"], "v1")
        self.assertNotIn("junk line", cfg)

    def test_parse_02(self):
        cfg = parse_config(["k2 = v2", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k2"], "v2")
        self.assertNotIn("junk line", cfg)

    def test_parse_03(self):
        cfg = parse_config(["k3 = v3", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k3"], "v3")
        self.assertNotIn("junk line", cfg)

    def test_parse_04(self):
        cfg = parse_config(["k4 = v4", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k4"], "v4")
        self.assertNotIn("junk line", cfg)

    def test_parse_05(self):
        cfg = parse_config(["k5 = v5", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k5"], "v5")
        self.assertNotIn("junk line", cfg)

    def test_parse_06(self):
        cfg = parse_config(["k6 = v6", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k6"], "v6")
        self.assertNotIn("junk line", cfg)

    def test_parse_07(self):
        cfg = parse_config(["k7 = v7", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k7"], "v7")
        self.assertNotIn("junk line", cfg)

    def test_parse_08(self):
        cfg = parse_config(["k8 = v8", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k8"], "v8")
        self.assertNotIn("junk line", cfg)

    def test_parse_09(self):
        cfg = parse_config(["k9 = v9", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k9"], "v9")
        self.assertNotIn("junk line", cfg)

    def test_parse_10(self):
        cfg = parse_config(["k10 = v10", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k10"], "v10")
        self.assertNotIn("junk line", cfg)

    def test_parse_11(self):
        cfg = parse_config(["k11 = v11", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k11"], "v11")
        self.assertNotIn("junk line", cfg)

    def test_parse_12(self):
        cfg = parse_config(["k12 = v12", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k12"], "v12")
        self.assertNotIn("junk line", cfg)

    def test_parse_13(self):
        cfg = parse_config(["k13 = v13", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k13"], "v13")
        self.assertNotIn("junk line", cfg)

    def test_parse_14(self):
        cfg = parse_config(["k14 = v14", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k14"], "v14")
        self.assertNotIn("junk line", cfg)

    def test_parse_15(self):
        cfg = parse_config(["k15 = v15", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k15"], "v15")
        self.assertNotIn("junk line", cfg)

    def test_parse_16(self):
        cfg = parse_config(["k16 = v16", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k16"], "v16")
        self.assertNotIn("junk line", cfg)

    def test_parse_17(self):
        cfg = parse_config(["k17 = v17", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k17"], "v17")
        self.assertNotIn("junk line", cfg)

    def test_parse_18(self):
        cfg = parse_config(["k18 = v18", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k18"], "v18")
        self.assertNotIn("junk line", cfg)

    def test_parse_19(self):
        cfg = parse_config(["k19 = v19", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k19"], "v19")
        self.assertNotIn("junk line", cfg)

    def test_parse_20(self):
        cfg = parse_config(["k20 = v20", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k20"], "v20")
        self.assertNotIn("junk line", cfg)

    def test_parse_21(self):
        cfg = parse_config(["k21 = v21", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k21"], "v21")
        self.assertNotIn("junk line", cfg)

    def test_parse_22(self):
        cfg = parse_config(["k22 = v22", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k22"], "v22")
        self.assertNotIn("junk line", cfg)

    def test_parse_23(self):
        cfg = parse_config(["k23 = v23", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k23"], "v23")
        self.assertNotIn("junk line", cfg)

    def test_parse_24(self):
        cfg = parse_config(["k24 = v24", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k24"], "v24")
        self.assertNotIn("junk line", cfg)

    def test_parse_25(self):
        cfg = parse_config(["k25 = v25", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k25"], "v25")
        self.assertNotIn("junk line", cfg)

    def test_parse_26(self):
        cfg = parse_config(["k26 = v26", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k26"], "v26")
        self.assertNotIn("junk line", cfg)

    def test_parse_27(self):
        cfg = parse_config(["k27 = v27", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k27"], "v27")
        self.assertNotIn("junk line", cfg)

    def test_parse_28(self):
        cfg = parse_config(["k28 = v28", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k28"], "v28")
        self.assertNotIn("junk line", cfg)

    def test_parse_29(self):
        cfg = parse_config(["k29 = v29", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k29"], "v29")
        self.assertNotIn("junk line", cfg)

    def test_parse_30(self):
        cfg = parse_config(["k30 = v30", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k30"], "v30")
        self.assertNotIn("junk line", cfg)

    def test_parse_31(self):
        cfg = parse_config(["k31 = v31", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k31"], "v31")
        self.assertNotIn("junk line", cfg)

    def test_parse_32(self):
        cfg = parse_config(["k32 = v32", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k32"], "v32")
        self.assertNotIn("junk line", cfg)

    def test_parse_33(self):
        cfg = parse_config(["k33 = v33", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k33"], "v33")
        self.assertNotIn("junk line", cfg)

    def test_parse_34(self):
        cfg = parse_config(["k34 = v34", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k34"], "v34")
        self.assertNotIn("junk line", cfg)

    def test_parse_35(self):
        cfg = parse_config(["k35 = v35", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k35"], "v35")
        self.assertNotIn("junk line", cfg)

    def test_parse_36(self):
        cfg = parse_config(["k36 = v36", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k36"], "v36")
        self.assertNotIn("junk line", cfg)

    def test_parse_37(self):
        cfg = parse_config(["k37 = v37", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k37"], "v37")
        self.assertNotIn("junk line", cfg)

    def test_parse_38(self):
        cfg = parse_config(["k38 = v38", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k38"], "v38")
        self.assertNotIn("junk line", cfg)

    def test_parse_39(self):
        cfg = parse_config(["k39 = v39", "# comment", "junk line", "x='quoted'"])
        self.assertEqual(cfg["k39"], "v39")
        self.assertNotIn("junk line", cfg)

    def test_validate_missing_key(self):
        self.assertFalse(validate_config({"a": 1}, ["a", "b"]))

    def test_validate_present(self):
        self.assertTrue(validate_config({"a": 1, "b": 2}, ["a", "b"]))

    def test_validate_empty(self):
        self.assertFalse(validate_config({}, ["a"]))

    def test_validate_none_value(self):
        self.assertFalse(validate_config({"a": None}, ["a"]))

    def test_user_age_none(self):
        self.assertFalse(validate_user({"name": "x", "age": None}))

    def test_user_negative_age(self):
        self.assertFalse(validate_user({"name": "x", "age": -1}))

    def test_user_200_age(self):
        self.assertFalse(validate_user({"name": "x", "age": 200}))

    def test_user_ok(self):
        self.assertTrue(validate_user({"name": "x", "age": 30}))

    def test_user_no_name(self):
        self.assertFalse(validate_user({"age": 30}))

    def test_table_short_row(self):
        out = render_table(["a", "b", "c"], [["1", "2"]])
        self.assertIn("1", out)

    def test_table_ok(self):
        out = render_table(["a", "b"], [["1", "2"], ["3", "4"]])
        self.assertEqual(len(out.splitlines()), 4)

    def test_table_empty(self):
        out = render_table(["a"], [])
        self.assertIn("a", out)

    def test_more_00(self):
        self.assertTrue(validate_user({"name": "u0", "age": 20}))

    def test_more_01(self):
        self.assertTrue(validate_user({"name": "u1", "age": 21}))

    def test_more_02(self):
        self.assertTrue(validate_user({"name": "u2", "age": 22}))

    def test_more_03(self):
        self.assertTrue(validate_user({"name": "u3", "age": 23}))

    def test_more_04(self):
        self.assertTrue(validate_user({"name": "u4", "age": 24}))

    def test_more_05(self):
        self.assertTrue(validate_user({"name": "u5", "age": 25}))

    def test_more_06(self):
        self.assertTrue(validate_user({"name": "u6", "age": 26}))

    def test_more_07(self):
        self.assertTrue(validate_user({"name": "u7", "age": 27}))

    def test_more_08(self):
        self.assertTrue(validate_user({"name": "u8", "age": 28}))

    def test_more_09(self):
        self.assertTrue(validate_user({"name": "u9", "age": 29}))

    def test_more_10(self):
        self.assertTrue(validate_user({"name": "u10", "age": 30}))

    def test_more_11(self):
        self.assertTrue(validate_user({"name": "u11", "age": 31}))

    def test_more_12(self):
        self.assertTrue(validate_user({"name": "u12", "age": 32}))

    def test_more_13(self):
        self.assertTrue(validate_user({"name": "u13", "age": 33}))

    def test_more_14(self):
        self.assertTrue(validate_user({"name": "u14", "age": 34}))

if __name__ == "__main__":
    unittest.main()
