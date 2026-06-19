"""hermes_demo — Hermes-3 + Headroom / Aphrodite proxy integration examples.

All examples target NousResearch/Hermes-3-Pro-Llama-3.1-8B running through
the Aphrodite OpenAI-compat proxy on http://127.0.0.1:9797/v1.

Environment variables
---------------------
APHRODITE_API_KEY   API key forwarded by the Aphrodite proxy (required).
HEADROOM_MODEL      Model slug; default NousResearch/Hermes-3-Pro-Llama-3.1-8B.
HEADROOM_PROXY_PORT Token-proxy port; default 9797.
HEADROOM_CACHE_PORT Cache-proxy port; default 9798.
"""

PROXY_PORT: int = 9797
CACHE_PORT: int = 9798
DEFAULT_MODEL: str = "NousResearch/Hermes-3-Pro-Llama-3.1-8B"
PROXY_BASE_URL: str = f"http://127.0.0.1:{PROXY_PORT}/v1"
