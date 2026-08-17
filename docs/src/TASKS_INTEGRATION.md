# Task Board Integration Checklist

**Status**: Core service complete (commit e5ac1f37), awaiting gateway/API/UI wiring

The `moltis-tasks` crate is now in the workspace with full CRUD operations. To complete Phase 3, integrate these components:

## Remaining Work

### 1. Gateway State & Migrations
- [ ] Add `TaskService` to `GatewayState` in `crates/gateway/src/state.rs`
- [ ] Call `moltis_tasks::run_migrations(pool).await` in startup sequence (e.g., `crates/gateway/src/startup.rs`)
- [ ] Add `mod tasks;` to gateway lib exports

### 2. API Routes (`crates/httpd/src/api/tasks.rs`)
```rust
// Create these endpoints:
POST   /api/tasks              // CreateTaskRequest → Task
GET    /api/tasks?status=open  // List with optional status filter
GET    /api/tasks/{id}         // Get single task with responses
PUT    /api/tasks/{id}         // UpdateTaskRequest → Task
POST   /api/tasks/{id}/responses  // AddResponseRequest → TaskResponse
```

Mount at:
```rust
// In crates/httpd/src/api.rs
.route("/tasks", post(create).get(list))
.route("/tasks/:id", get(get).put(update))
.route("/tasks/:id/responses", post(add_response))
```

### 3. Web UI (`crates/web/ui/src/pages/tasks/`)
- [ ] Create `pages/tasks/index.tsx` — Kanban board view
- [ ] Create `pages/tasks/task-detail-modal.tsx` — detail/responses
- [ ] Update SPA routes in `crates/web/src/templates.rs` to include `/tasks`
- [ ] Add task board icon to main nav
- [ ] Style with Tailwind (use existing card/badge patterns)

### 4. Agent Integration
Register as tools in `crates/tools/src/registry.rs`:
```rust
tasks_list(status?: "open" | "assigned" | "in_progress" | "completed") → Task[]
tasks_get(id: string) → Task
tasks_update(id: string, status: "open" | "assigned" | "in_progress" | "completed", assigned_to?: string) → Task
tasks_add_response(id: string, content: string, agent_name: string) → TaskResponse
```

Agent workflow:
1. Check available work: `tasks_list(status="open")`
2. Claim task: `tasks_update(id, status="assigned", assigned_to="agent-name")`
3. Start work: `tasks_update(id, status="in_progress")`
4. Post updates: `tasks_add_response(id, content="working on X...")`
5. Complete: `tasks_update(id, status="completed")`

### 5. Build & Test
- [ ] `cargo check --bin moltis` passes
- [ ] Run `cargo test -p moltis-tasks` (service tests included)
- [ ] Add E2E test: create task, list tasks, update status
- [ ] Test agent can query & update tasks via tools

## Files Already Complete

✅ `crates/tasks/src/lib.rs` — exports, migrations runner
✅ `crates/tasks/src/types.rs` — Task, TaskStatus, DTOs
✅ `crates/tasks/src/service.rs` — CRUD + tests
✅ `crates/tasks/src/error.rs` — error types
✅ `crates/tasks/migrations/20260817000000_tasks.sql` — schema
✅ `crates/tasks/Cargo.toml` — dependencies
✅ Workspace wiring in root `Cargo.toml`

## Time Estimate

- Gateway state & migrations: 15 min
- API routes: 20 min
- Web UI: 30 min
- Agent tools: 15 min
- Testing & polish: 20 min
- **Total: ~100 min (1.5-2 hours)**

## Testing the Task Crate

```bash
# Run service tests
cargo test -p moltis-tasks

# Check dependencies resolve
cargo check -p moltis-tasks
```

## Notes

- Task board is global (not session-scoped like chat)
- Responses attribute to agent_name for audit trail
- Migrations run automatically on startup
- API follows moltis REST conventions (POST, GET, PUT)
- Priority: numeric (lower = higher; default 0)
- Status transitions: open → assigned → in_progress → completed (or closed)
