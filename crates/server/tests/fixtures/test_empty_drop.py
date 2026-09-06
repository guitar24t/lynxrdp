import importlib.util
from pathlib import Path
import unittest

path = Path(__file__).parents[2] / "src/x11/empty_drop.py"
spec = importlib.util.spec_from_file_location("empty_drop", path)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class EmptyPaneTargetTests(unittest.TestCase):
    window = (392, 128, 942, 602)
    pane = (607, 198, 701, 503)

    def test_placeholder_retry_stays_in_the_same_empty_file_pane(self):
        point = helper.candidate([(self.pane, True)], 961, 510, self.window)
        self.assertEqual(point, [961, 676])
        self.assertTrue(helper.inside(self.pane, *point))

    def test_does_not_redirect_sidebar_or_toolbar_drops(self):
        for point in [(500, 510), (961, 170)]:
            self.assertIsNone(helper.candidate([(self.pane, True)], *point, self.window))

    def test_nonempty_view_does_not_get_an_alternate_target(self):
        self.assertIsNone(helper.candidate([(self.pane, False)], 961, 510, self.window))

    def test_ambiguous_or_foreign_views_are_rejected(self):
        self.assertIsNone(helper.candidate([(self.pane, True)] * 2, 961, 510, self.window))
        self.assertIsNone(helper.candidate([((0, 0, 2000, 2000), True)], 961, 510, self.window))


if __name__ == "__main__":
    unittest.main()
