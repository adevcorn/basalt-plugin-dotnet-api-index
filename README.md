# basalt-plugin-dotnet-api-index

Heuristic API endpoint indexing plugin for `.NET` workspaces in Basalt.

## Capabilities

- `provides: api-index:dotnet`
- `requires: project-model:dotnet`
- `optional_requires: semantic-query:dotnet`

## Current Behavior & Features

- **ASP.NET Controller Indexing**: Scans C# source files for controller classes, detecting route templates via `[Route(...)]` and HTTP verb attributes (`[HttpGet]`, `[HttpPost]`, `[HttpPut]`, `[HttpDelete]`, `[HttpPatch]`, `[HttpHead]`, `[Options]`).
- **Minimal API Indexing**: Detects minimal API endpoint mappings (`MapGet`, `MapPost`, `MapPut`, `MapDelete`, `MapPatch`, `MapMethods`, `MapFallback`, `MapGroup`).
- **Basalt Schema Export**: Generates and emits Basalt's standard API-index JSON schema for consumption by the host environment.

## Usage & Integration

This crate compiles to a `cdylib` plugin targeting the Basalt plugin system.

```bash
cargo build --release
```

## Planned Upgrades

- **Semantic Query Integration**: When `semantic-query:dotnet` becomes available via a `.NET` LSP plugin, this plugin will refine grouped routes, constant resolution, and indirect route registrations while maintaining full backwards compatibility with the `api-index:dotnet` contract.
