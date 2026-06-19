"""``ImageCompressionDecision``: canonical "should this request have
images compressed?" gate.

Mirror of :class:`CompressionDecision` (text compression) and
:class:`MemoryDecision`. Pre-this-PR image compression was gated at
two sites (``openai.py:1203``, ``anthropic.py:868``) by inline
conjunctions. Both already checked ``_bypass`` (no drift bug like
the text-compression Gemini bypass-misses) - but consolidating into
a value type still pays off:

* Locks the contract via tests so a future site can't drift
* ``apply_to_tags()`` surfaces ``image_skip_reason`` to
  :class:`RequestOutcome.tags` for dashboard slicing
* Rust-portable shape, same as the other decision types

Precedence (highest first):

  1. ``bypass_header``           - user's explicit opt-out
  2. ``image_optimize_disabled`` - operator ``config.image_optimize=False``
  3. ``no_messages``             - empty / missing messages
  4. ``auto_fill_low``           - auto mode, fill_ratio below threshold
  5. otherwise -> ``should_compress=True``

Distinct from text :class:`CompressionDecision`'s
``compression_disabled`` reason: operators can enable text + disable
image (or vice versa) independently. Same shape, different gate.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
from typing import Any

from headroom.proxy.helpers import _headroom_bypass_enabled

# When image_optimize is set to auto mode, images are only compressed
# if the context fill ratio reaches this threshold, preventing wasted
# compression on requests with sparse image content.
AUTO_IMAGE_THRESHOLD: float = 0.70


@dataclass(frozen=True)
class ImageCompressionDecision:
    """Immutable, value-equal snapshot of the image-compression gate.

    Construction policy: use :meth:`decide`. Direct construction is
    allowed for tests but unusual - handlers always go through
    ``decide``. The constituent observability booleans
    (``bypass_header_set`` etc.) MUST match the inputs ``decide`` saw;
    the factory enforces that invariant, and the dataclass being
    frozen means downstream code can't violate it.
    """

    should_compress: bool
    # When ``should_compress`` is False, the canonical reason surfaced
    # in ``RequestOutcome.tags["image_skip_reason"]`` so the dashboard
    # can slice image-skipped traffic by cause. One of:
    #   * "bypass_header"            - user set x-headroom-bypass/mode
    #   * "image_optimize_disabled"  - operator config off
    #   * "no_messages"              - empty / missing messages
    #   * "auto_fill_low"            - auto mode, fill_ratio below threshold
    # When ``should_compress`` is True, this is None.
    passthrough_reason: str | None

    # Observability: every constituent boolean exposed so debug tools
    # answer "what did the decision see?" without re-running it.
    bypass_header_set: bool
    image_optimize_enabled: bool
    image_optimize_auto: bool
    has_messages: bool

    @classmethod
    def decide(
        cls,
        *,
        headers: Any,
        config: Any,
        messages: Sequence[Any] | None,
        fill_ratio: float | None = None,
    ) -> ImageCompressionDecision:
        """Compute the canonical image-compression decision.

        Parameters
        ----------
        headers
            Inbound request headers. Accepts any object with a
            ``.get(key)`` method (dict, starlette Headers, mapping).
            Bypass detected via ``_headroom_bypass_enabled``.
        config
            ``HeadroomConfig``-shaped object; ``image_optimize`` and
            ``image_optimize_auto`` are read.
        messages
            Request messages. ``None`` and ``[]`` are equivalent.
        fill_ratio
            Context fill ratio (0.0-1.0). Only meaningful when auto
            mode is active - if provided and below
            ``AUTO_IMAGE_THRESHOLD``, compression is skipped.
        """
        bypass = _headroom_bypass_enabled(headers)
        image_ok = bool(getattr(config, "image_optimize", False))
        image_auto = bool(getattr(config, "image_optimize_auto", False))
        has_msgs = bool(messages)

        if bypass:
            reason: str | None = "bypass_header"
            should = False
        elif not image_ok:
            reason = "image_optimize_disabled"
            should = False
        elif not has_msgs:
            reason = "no_messages"
            should = False
        elif image_auto and fill_ratio is not None and fill_ratio >= AUTO_IMAGE_THRESHOLD:
            reason = "auto_fill_low"
            should = False
        else:
            reason = None
            should = True

        return cls(
            should_compress=should,
            passthrough_reason=reason,
            bypass_header_set=bypass,
            image_optimize_enabled=image_ok,
            image_optimize_auto=image_auto,
            has_messages=has_msgs,
        )

    def apply_to_tags(self, tags: dict[str, str]) -> None:
        """Stamp the skip reason into a tags dict for dashboard slicing.

        Mutates ``tags`` in place. No-op when ``should_compress=True``
        - absence vs presence is the signal.

        Mirror of :meth:`CompressionDecision.apply_to_tags` and
        :meth:`MemoryDecision.apply_to_tags`. Multiple decision tags
        coexist in the same dict (``passthrough_reason``,
        ``memory_skip_reason``, ``image_skip_reason``) for full
        dashboard slicing.
        """
        if self.passthrough_reason is not None:
            tags["image_skip_reason"] = self.passthrough_reason
