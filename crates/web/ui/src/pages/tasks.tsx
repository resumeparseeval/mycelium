import { render } from "preact";
import { signal } from "@preact/signals";
import { useEffect } from "preact/hooks";
import { Badge } from "../components/forms";
import { registerPrefix } from "../router";
import { routes } from "../routes";
import { type Task, TaskStatus } from "../types/tasks";

interface TasksPageProps {
  initialTasks?: Task[];
}

export function TasksPage(props: TasksPageProps) {
  const tasks = signal<Task[]>(props.initialTasks || []);
  const loading = signal(false);
  const error = signal<string | null>(null);
  const filter = signal<TaskStatus | "all">("all");

  useEffect(() => {
    loadTasks();
  }, []);

  async function loadTasks() {
    loading.value = true;
    error.value = null;
    try {
      const params = filter.value === "all" ? "" : `?status=${filter.value}`;
      const res = await fetch(`/api/tasks${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      tasks.value = await res.json();
    } catch (e) {
      error.value = e instanceof Error ? e.message : "Failed to load tasks";
    } finally {
      loading.value = false;
    }
  }

  const tasksByStatus = () => {
    const grouped: Record<TaskStatus, Task[]> = {
      open: [],
      assigned: [],
      in_progress: [],
      completed: [],
      closed: [],
    };
    tasks.value.forEach((t) => {
      grouped[t.status].push(t);
    });
    return grouped;
  };

  const updateTaskStatus = async (taskId: string, newStatus: TaskStatus) => {
    try {
      const res = await fetch(`/api/tasks/${taskId}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ status: newStatus }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const updated = await res.json();
      tasks.value = tasks.value.map((t) => (t.id === taskId ? updated : t));
    } catch (e) {
      error.value = e instanceof Error ? e.message : "Failed to update task";
    }
  };

  return (
    <div class="min-h-screen bg-gray-50 p-6">
      <div class="max-w-7xl mx-auto">
        <div class="mb-6">
          <h1 class="text-3xl font-bold text-gray-900">Tasks</h1>
          <p class="text-gray-600 mt-1">Track work items and agent progress</p>
        </div>

        {error.value && (
          <div class="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg">
            <p class="text-red-800">{error.value}</p>
          </div>
        )}

        <div class="mb-4 flex gap-2">
          {(["all", "open", "assigned", "in_progress", "completed", "closed"] as const).map(
            (status) => (
              <button
                key={status}
                onClick={() => {
                  filter.value = status;
                  loadTasks();
                }}
                class={`px-4 py-2 rounded-lg font-medium transition-colors ${
                  filter.value === status
                    ? "bg-blue-600 text-white"
                    : "bg-white text-gray-700 border border-gray-200 hover:bg-gray-50"
                }`}
              >
                {status === "in_progress" ? "In Progress" : status.charAt(0).toUpperCase() + status.slice(1)}
              </button>
            )
          )}
        </div>

        {loading.value ? (
          <div class="flex items-center justify-center py-12">
            <div class="text-gray-600">Loading tasks...</div>
          </div>
        ) : (
          <div class="grid grid-cols-1 md:grid-cols-5 gap-4">
            {["open", "assigned", "in_progress", "completed", "closed"].map((status) => (
              <TaskColumn
                key={status}
                status={status as TaskStatus}
                tasks={tasksByStatus()[status as TaskStatus] || []}
                onStatusChange={updateTaskStatus}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

interface TaskColumnProps {
  status: TaskStatus;
  tasks: Task[];
  onStatusChange: (taskId: string, status: TaskStatus) => Promise<void>;
}

function TaskColumn(props: TaskColumnProps) {
  const statusLabels: Record<TaskStatus, string> = {
    open: "Open",
    assigned: "Assigned",
    in_progress: "In Progress",
    completed: "Completed",
    closed: "Closed",
  };

  const statusColors: Record<TaskStatus, string> = {
    open: "bg-gray-100",
    assigned: "bg-blue-50",
    in_progress: "bg-yellow-50",
    completed: "bg-green-50",
    closed: "bg-gray-100",
  };

  const statusBadgeColors: Record<TaskStatus, string> = {
    open: "bg-gray-200 text-gray-700",
    assigned: "bg-blue-200 text-blue-700",
    in_progress: "bg-yellow-200 text-yellow-700",
    completed: "bg-green-200 text-green-700",
    closed: "bg-gray-300 text-gray-800",
  };

  return (
    <div class={`${statusColors[props.status]} rounded-lg p-4 flex flex-col`}>
      <div class="mb-4">
        <h2 class="font-semibold text-gray-900 text-sm">{statusLabels[props.status]}</h2>
        <p class="text-xs text-gray-600 mt-1">{props.tasks.length} items</p>
      </div>

      <div class="flex-1 space-y-3">
        {props.tasks.map((task) => (
          <TaskCard key={task.id} task={task} status={props.status} onStatusChange={props.onStatusChange} />
        ))}
      </div>
    </div>
  );
}

interface TaskCardProps {
  task: Task;
  status: TaskStatus;
  onStatusChange: (taskId: string, status: TaskStatus) => Promise<void>;
}

function TaskCard(props: TaskCardProps) {
  const nextStatuses: Record<TaskStatus, TaskStatus[]> = {
    open: ["assigned", "closed"],
    assigned: ["in_progress", "open"],
    in_progress: ["completed", "assigned"],
    completed: ["closed", "open"],
    closed: ["open"],
  };

  return (
    <div class="bg-white rounded-lg p-3 shadow-sm border border-gray-200 hover:shadow-md transition-shadow">
      <div class="flex-1">
        <h3 class="font-medium text-sm text-gray-900 truncate">{props.task.title}</h3>
        {props.task.description && (
          <p class="text-xs text-gray-600 mt-1 line-clamp-2">{props.task.description}</p>
        )}
      </div>

      <div class="mt-3 space-y-2">
        {props.task.assigned_to && (
          <Badge variant="secondary" size="sm">
            {props.task.assigned_to}
          </Badge>
        )}

        {props.task.responses.length > 0 && (
          <p class="text-xs text-gray-500">{props.task.responses.length} responses</p>
        )}
      </div>

      <div class="mt-3 flex gap-2 flex-wrap">
        {nextStatuses[props.status].map((nextStatus) => (
          <button
            key={nextStatus}
            onClick={() => props.onStatusChange(props.task.id, nextStatus)}
            class="text-xs px-2 py-1 rounded bg-gray-100 text-gray-700 hover:bg-gray-200 transition-colors"
          >
            {nextStatus === "in_progress" ? "Start" : nextStatus.charAt(0).toUpperCase() + nextStatus.slice(1)}
          </button>
        ))}
      </div>
    </div>
  );
}

registerPrefix(routes.tasks!, (container: HTMLElement) => {
  render(<TasksPage />, container);
});
