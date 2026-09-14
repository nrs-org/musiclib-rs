# Deduplication annotation prompt v2

You are labeling possible duplicate music-library entities. Judge only the
evidence in each task. Do not search the web, infer from the sampling process,
or assume that identical names mean identical entities. Abstaining is useful:
use `insufficient_evidence` when the packet cannot support a factual decision.

Each input item has a `task` and `presented_order`. Displayed sides may be
swapped from canonical order to measure side bias. Copy `presented_order`
exactly. Refer to displayed entities by their `view_id`.

## Identity

Choose exactly one `factual.entity_relation`:

- `same_identity`: two source views of one entity at the task's granularity.
- `different_identity`: distinct entities, whether related or unrelated.
- `insufficient_evidence`: the packet cannot establish either conclusion.

Entity boundaries depend on `entry_type`:

- `track`: the same recording-level library entity. A materially different
  performance, mix, edit, cover, or instrumental remains a distinct identity.
- `release`: the same issued edition/listing. Format and territory editions,
  reissues, deluxe editions, and materially different tracklists remain
  distinct release identities.
- `release_group`: the same conceptual release project.
- `artist`: the same credited artistic identity. Members, units,
  collaborations, personas, and successor projects remain distinct unless the
  evidence establishes that they are merely names for one identity.

Absence of corroboration is not evidence for `different_identity`.

## Primitive relationships

When distinct entities are related, express the relationship using only these
directed primitives in `factual.relations`:

- `member_of`: one or both subjects belong to the object group, family, or
  containing entity.
- `derived_from`: each subject was produced by transforming or continuing the
  object.
- `participates_in`: each subject contributes to or participates in the object.
- `facet_of`: each subject is a separately modeled public identity representing
  the object identity.

Rich distinctions belong in the relation's open `metadata` object, not in new
predicates. Examples include `{"transformation":"remix"}` or
`{"membership":"edition","territory":"JP"}`.

Every relation has one or two `subjects` and one `object`. A reference contains:

- `ref_kind: "view"` and the exact task `view_id` for a displayed entity;
- `ref_kind: "provider"` for an explicitly identified external entity; or
- `ref_kind: "hypothesis"` for an implied but unidentified common parent, using
  an annotation-local stable ref such as `hypothesis:shared-release-group:1`.

For two releases in one release group, create one `member_of` assertion with
both release views as subjects and the release group as object. This expands to
two stored graph edges. Tracklist differences are evidence that the releases
are distinct; they are metadata, not a `tracklist_variant` predicate.

Common mappings:

- edit/remaster/remix/instrumental -> `derived_from`, with transformation metadata;
- live/cover/other performance of one work -> `member_of` a work or recording family;
- release editions -> `member_of` a release group;
- artist member/unit -> `member_of` the group;
- artist in a collaboration -> `participates_in` the collaboration;
- persona -> `facet_of` the underlying identity;
- renamed project -> `same_identity` if only the name changed, otherwise
  `derived_from` when genuine successor lineage is supported.

Do not record vague relatedness. If no concrete primitive is supported, leave
`relations` empty. `same_identity` and `insufficient_evidence` always require an
empty relationship list.

Each primitive relation has `evidence_refs`; these must cite fact evidence IDs
from the packet. The judgment also has global `citations` for the identity
decision.

## Name relation and policy

Label the displayed names independently as `same_string`,
`orthographic_variant`, `transliteration`, `translation`, `abbreviation`,
`generic_collision`, `unrelated`, `mixed`, or `unclear`. A transliteration is
candidate evidence, not proof of identity.

Policy is derived mechanically:

- `same_identity` -> `merge`
- `different_identity` with one or more primitive relations -> `relate`
- `different_identity` with no primitive relations -> `keep_separate`
- `insufficient_evidence` -> `defer`

Set `exception_reason` to `null` unless a genuine product-policy exception is
present.

## Evidence and output

Cite fact `evidence_id` values from both sides when possible. Global citation
roles name the claim rather than being relative to the final verdict:
`supports_same_identity`, `supports_different_identity`,
`supports_primitive_relation`, or `context`. This lets an abstention cite
evidence pointing in both directions without calling either fact a generic
contradiction. Never invent an ID. In this closed-book pass,
`external_evidence` must be `[]`. Use `packet_insufficient` when missing evidence
prevents judgment. Use `left_mixed` or `right_mixed` only when a view appears to
combine multiple identities.

`source_classification` is provider-scoped: its vocabulary distinguishes album
types, statuses, and collection types. `parent_release`,
`parent_release_group`, `credited_on`, and `parent_entity` are distinct
structural relationships.

Return JSONL only: one annotation per task, in input order, with no Markdown or
commentary. Output must validate against the `annotation` definition in
`docs/dedup-label-schema-v2.json`. Structural example:

```json
{"record_type":"annotation","schema_version":"dedup-calibration/2","annotation_id":"<unique id>","item_id":"<task item_id>","created_at":"<RFC3339 UTC>","presented_order":["left","right"],"blind_to_hidden":true,"annotator":{"kind":"model","annotator_id":"<assigned id>","provider":"<provider>","model":"<model>","prompt_version":"dedup-label-prompt/2","run_id":"<assigned run>"},"judgment":{"input_integrity":"ok","factual":{"entity_relation":"different_identity","relations":[]},"name_relation":{"primary":"unrelated","secondary":[]},"policy":{"action":"keep_separate","exception_reason":null},"confidence":{"label_probability":0.8,"evidence_quality":"medium"},"external_evidence":[],"citations":[{"ref":"L.1.alias.1","role":"supports_different_identity"},{"ref":"R.1.alias.1","role":"supports_different_identity"}],"rationale":"Concise packet-grounded explanation."}}
```

The example is structural, not a default label. Confidence is the probability
that the complete factual judgment is correct, not name similarity.
