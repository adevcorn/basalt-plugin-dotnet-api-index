# basalt-plugin-dotnet-api-index

Heuristic API endpoint indexing for `.NET` workspaces in Basalt.

Capabilities:
- `provides: api-index:dotnet`
- `requires: project-model:dotnet`
- `optional_requires: semantic-query:dotnet`

Current behavior:
- indexes ASP.NET controller endpoints from route and `Http*` attributes
- indexes minimal API calls like `MapGet`, `MapPost`, `MapPut`, `MapDelete`, `MapPatch`
- emits Basalt's generic API-index JSON schema

Planned semantic upgrade path:
- if a future `.NET` LSP plugin exposes `semantic-query:dotnet`, this plugin can
  refine grouped routes, constants, and indirect registrations without changing
  its host-facing `api-index:dotnet` contract
