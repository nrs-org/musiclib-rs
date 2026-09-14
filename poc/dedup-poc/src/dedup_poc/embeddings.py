from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .core import View


DEFAULT_MODEL = "sentence-transformers/LaBSE"


def embedding_text(view: View) -> str:
    """Build model input exclusively from fields exported by musiclib-rs."""
    sections = [
        sorted(view.names),
        sorted(view.artist_names),
        sorted(view.parent_release_groups),
        sorted(view.parent_releases),
        view.track_titles[:20],
    ]
    values: list[str] = []
    seen = set()
    for section in sections:
        for value in section:
            if value and value not in seen:
                values.append(value)
                seen.add(value)
    return " ; ".join(values)


def embed_views(
    views: dict[str, View],
    *,
    model_name: str = DEFAULT_MODEL,
    batch_size: int = 32,
    allow_download: bool = False,
) -> dict[str, list[float]]:
    try:
        from sentence_transformers import SentenceTransformer
    except ImportError as error:
        raise RuntimeError(
            "semantic dependencies are missing; run with `uv run --extra semantic`"
        ) from error

    ordered = [
        (view, text)
        for view in sorted(views.values(), key=lambda view: view.view_id)
        if (text := embedding_text(view))
    ]
    model = SentenceTransformer(model_name, local_files_only=not allow_download)
    vectors = model.encode(
        [text for _, text in ordered],
        batch_size=batch_size,
        normalize_embeddings=True,
        show_progress_bar=True,
    )
    result = {}
    for (view, _), vector in zip(ordered, vectors, strict=True):
        values = vector.tolist()
        view.semantic_vector = values
        result[view.view_id] = values
    return result
