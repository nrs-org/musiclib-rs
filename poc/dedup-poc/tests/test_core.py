import unittest

from dedup_poc.core import (
    LogisticModel,
    FEATURE_NAMES,
    View,
    features,
    generate_candidates,
    kana_to_romaji,
    normalize,
    romanize_japanese,
)


class CoreTests(unittest.TestCase):
    def test_normalize_nfkc_and_punctuation(self):
        self.assertEqual(normalize("ＡＢＣ—Live!"), "abc live")

    def test_kana_romanization(self):
        self.assertEqual(kana_to_romaji("キセキ"), "kiseki")
        self.assertEqual(kana_to_romaji("わため"), "watame")
        self.assertEqual(romanize_japanese("音楽"), "ongaku")

    def test_version_conflict_is_a_negative_feature(self):
        left = View("left", "track", "Song", [])
        left.names = {"song"}
        right = View("right", "track", "Song (live)", [])
        right.names = {"song live"}
        right.markers = {"live"}
        self.assertEqual(features(left, right)["version_conflict"], 1.0)

    def test_model_keeps_evidence_directions_monotonic(self):
        model = LogisticModel()
        width = len(FEATURE_NAMES)
        model.fit([[0.0] * width, [1.0] * width], [1, 0], epochs=20)
        coefficients = model.coefficients()
        self.assertTrue(
            all(
                value >= 0
                for key, value in coefficients.items()
                if key not in {
                    "version_conflict", "qualifier_conflict", "primary_type_conflict",
                    "role_credit_conflict", "internal_mixedness", "empty_side",
                }
            )
        )
        self.assertTrue(
            all(
                coefficients[key] <= 0
                for key in {
                    "version_conflict", "qualifier_conflict", "primary_type_conflict",
                    "role_credit_conflict", "internal_mixedness", "empty_side",
                }
            )
        )

    def test_exported_model_matches_native_probability(self):
        model = LogisticModel()
        width = len(FEATURE_NAMES)
        rows = [[0.0] * width, [1.0] * width, [0.25] * width]
        model.fit(rows, [0, 1, 0], epochs=20)
        artifact = model.export()
        row = [0.4] * width
        logit = artifact["intercept"] + sum(
            artifact["coefficients"][name] * value
            for name, value in zip(FEATURE_NAMES, row, strict=True)
        )
        portable_probability = LogisticModel._sigmoid(logit)
        self.assertAlmostEqual(model.predict(row), portable_probability, places=12)

    def test_duration_credit_retrieval_is_bounded_and_structural(self):
        left = View("left", "track", "異なる題", [])
        left.names = {"異なる題"}
        left.artist_names = {"artist"}
        left.durations = {180_000}
        right = View("right", "track", "different title", [])
        right.names = {"different title"}
        right.artist_names = {"artist"}
        right.durations = {183_000}
        candidates = generate_candidates(
            {view.view_id: view for view in (left, right)}, candidate_k=1
        )
        self.assertIn(("left", "right"), candidates)
        self.assertIn("duration_credit", candidates[("left", "right")])

    def test_release_tracklist_overlap_retrieval(self):
        left = View("left", "release", "Edition A", [])
        left.names = {"edition a"}
        left.track_titles = ["one", "two", "three"]
        right = View("right", "release", "Edition B", [])
        right.names = {"edition b"}
        right.track_titles = ["one", "two", "bonus"]
        candidates = generate_candidates(
            {view.view_id: view for view in (left, right)}, candidate_k=2
        )
        self.assertIn("tracklist_overlap", candidates[("left", "right")])

    def test_zero_candidate_cap_keeps_channel_bounded_union(self):
        views = {}
        for view_id in ("a", "b", "c"):
            view = View(view_id, "artist", "Shared", [])
            view.names = {"shared"}
            view.romanized_names = {"shared"}
            view.base_names = {"shared"}
            views[view_id] = view
        candidates = generate_candidates(views, candidate_k=0)
        self.assertEqual(set(candidates), {("a", "b"), ("a", "c"), ("b", "c")})


if __name__ == "__main__":
    unittest.main()
