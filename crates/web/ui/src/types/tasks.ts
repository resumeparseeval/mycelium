export type TaskStatus = "open" | "assigned" | "in_progress" | "completed" | "closed";

export interface TaskResponse {
  id: string;
  task_id: string;
  agent_name: string;
  content: string;
  created_at: string;
}

export interface Task {
  id: string;
  title: string;
  description?: string;
  status: TaskStatus;
  assigned_to?: string;
  priority: number;
  created_at: string;
  updated_at: string;
  completed_at?: string;
  responses: TaskResponse[];
}

export interface CreateTaskRequest {
  title: string;
  description?: string;
  priority?: number;
}

export interface UpdateTaskRequest {
  title?: string;
  description?: string;
  status?: TaskStatus;
  assigned_to?: string;
  priority?: number;
}

export interface AddResponseRequest {
  agent_name: string;
  content: string;
}
