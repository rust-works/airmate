# Architecture decision records

One record per decision that shaped airmate and would be expensive to reverse. Each says what the
situation was, what was decided, what that costs, and what was rejected.

| # | Decision |
|---|---|
| [1](0001-default-closed-surface.md) | The surface is default-closed |
| [2](0002-classed-field-policy.md) | Every field is classed: translated, ignored or rejected |
| [3](0003-protocol-neutral-ir.md) | One protocol-neutral intermediate representation |
| [4](0004-signed-thinking-blocks.md) | Reasoning is returned in signed blocks |
| [5](0005-failover-semantics.md) | Failover stops at the first byte |
| [6](0006-config-and-secrets.md) | YAML configuration that refers to secrets and never holds them |
| [7](0007-admin-socket.md) | Administration is on a Unix-domain socket, not the data-plane port |
