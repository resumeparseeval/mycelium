# Phase 3: Task Board Implementation

**Status**: Planned for follow-up session  
**Scope**: Add task queue/work board interface for agents  
**Estimate**: ~2-3 hours

## Overview

Task board is a different interaction modality than chat:
- **Chat**: sequential conversation flow, one message stream
- **Tasks**: parallel work items with status tracking, responses per task

Users post tasks to a board. Agents query available tasks, work on them, and post responses/updates. Multiple tasks can be in-flight simultaneously with clear ownership and progress tracking.

## Implementation Plan

### 1. Database Schema (`crates/tasks/migrations/`)
- `tasks` table: id, title, description, status, assigned_to, priority, created_at, updated_at, completed_at
- `task_responses` table: id, task_id, agent_name, content, created_at
- Indices on status, assigned_to for fast queries

### 2. Task Service (`crates/tasks/src/`)
- `types.rs`: Task, TaskResponse, TaskStatus enum, request DTOs
- `service.rs`: TaskService with CRUD operations
- `error.rs`: Error types
- Full test coverage (list, create, update, add_response, state transitions)

### 3. API Routes (`crates/httpd/src/tasks/`)
- `POST /api/tasks` — create task
- `GET /api/tasks?status=open` — list (filter by status optional)
- `GET /api/tasks/{id}` — get task with responses
- `PUT /api/tasks/{id}` — update (title, status, assigned_to, priority)
- `POST /api/tasks/{id}/responses` — add agent response
- Content negotiation: JSON

### 4. Web UI (`crates/web/ui/src/pages/tasks/`)
- Task board view: columns for open, assigned, in-progress, completed
- Task card: title, description, assignee, priority, response count
- Task detail modal: full description, all responses, update controls
- Drag-to-update status (Kanban-style optional, simpler: dropdown OK)
- Create new task form
- Filter by status/assignee

### 5. Agent Integration
Register tasks as a tool agents can use:
- `tasks_list(status: Optional[TaskStatus])` → [Task]
- `tasks_get(id: String)` → Task
- `tasks_update(id: String, status: TaskStatus)` → Task
- `tasks_add_response(id: String, content: String)` → TaskResponse

Agents can:
- Check what work is available: `tasks_list(status="open")`
- Claim a task: `tasks_update(id, status="assigned", assigned_to="agent-name")`
- Update progress: `tasks_update(id, status="in_progress")`
- Post updates: `tasks_add_response(id, content="...progress...")`
- Complete: `tasks_update(id, status="completed")`

### 6. Configuration
- Add `[tasks]` section to config (enabled by default, can disable)
- Optional: rate limits, max task count, retention policy

## Testing
- Unit tests in TaskService (list, create, update, transitions)
- Integration tests (E2E task flow: create → assign → complete)
- E2E test in Playwright: create task, agent claims it, agent updates, mark complete

## Migration Path
1. Add crate to workspace → runs migrations automatically on startup
2. API routes appear at `/api/tasks/...`
3. Task board UI available at `/tasks` once compiled
4. Agent tools registered in tool registry (automatic)

## Success Criteria
- [ ] Task CRUD fully working
- [ ] Agent can list tasks via tool
- [ ] Agent can update task status and add responses
- [ ] Web UI shows all tasks with Kanban-style board
- [ ] Tests cover happy path + error cases
- [ ] Moltis builds and runs without matrix-sdk

## Notes
- Tasks are session-agnostic (unlike chat which is per-session)
- Responses are attributed to agent (agent_name field)
- Status enum: open → assigned → in_progress → completed (or closed)
- Priority is numeric, lower = higher priority (consistent with other systems)
- created_at immutable; updated_at changes on any update; completed_at set when status=completed
