"""Headroom integrations — coding-agent focused.

MCP (Model Context Protocol):
    - HeadroomMCPCompressor: Compress MCP tool results
    - compress_tool_result: Simple function for tool compression
    - HeadroomMCPClientWrapper: Wrapped MCP client with compression
    - create_headroom_mcp_proxy: Create MCP proxy with compression

Example:
    from headroom.integrations import compress_tool_result
    from headroom.integrations.mcp import compress_tool_result
"""

from .mcp import (
    DEFAULT_MCP_PROFILES,
    HeadroomMCPClientWrapper,
    HeadroomMCPCompressor,
    MCPCompressionResult,
    MCPToolProfile,
    compress_tool_result,
    compress_tool_result_with_metrics,
    create_headroom_mcp_proxy,
)

__all__ = [
    "HeadroomMCPCompressor",
    "HeadroomMCPClientWrapper",
    "MCPCompressionResult",
    "MCPToolProfile",
    "compress_tool_result",
    "compress_tool_result_with_metrics",
    "create_headroom_mcp_proxy",
    "DEFAULT_MCP_PROFILES",
]
