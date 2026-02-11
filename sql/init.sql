-- Session Manager PostgreSQL Setup
-- Run this as a superuser/admin on your shared database

-- Create role for session-manager application
CREATE ROLE session_manager_app WITH LOGIN PASSWORD 'CHANGE_ME_TO_SECURE_PASSWORD';

-- Create the schema owned by session_manager_app
CREATE SCHEMA IF NOT EXISTS session_manager AUTHORIZATION session_manager_app;

-- Grant schema usage and creation rights
GRANT USAGE ON SCHEMA session_manager TO session_manager_app;
GRANT CREATE ON SCHEMA session_manager TO session_manager_app;

-- Grant permissions on all current and future tables in the schema
GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA session_manager TO session_manager_app;
GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA session_manager TO session_manager_app;

-- Ensure future tables/sequences are also accessible
ALTER DEFAULT PRIVILEGES IN SCHEMA session_manager
    GRANT ALL PRIVILEGES ON TABLES TO session_manager_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA session_manager
    GRANT ALL PRIVILEGES ON SEQUENCES TO session_manager_app;

-- Restrict this role from accessing other schemas (like mattermost's public schema)
-- By default, PostgreSQL grants USAGE on public schema to all roles, so revoke it
REVOKE ALL ON SCHEMA public FROM session_manager_app;

-- Containers table: tracks devcontainers independently from sessions
-- Multiple sessions can share a single container (one-container-per-repo model)
CREATE TABLE IF NOT EXISTS session_manager.containers (
    id BIGSERIAL PRIMARY KEY,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL DEFAULT '',
    container_name TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'running',
    session_count INTEGER NOT NULL DEFAULT 0,
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    devcontainer_json_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(repo, branch)
);

CREATE INDEX IF NOT EXISTS idx_containers_repo_branch
    ON session_manager.containers(repo, branch);

-- Migration: add container_id to sessions table
-- ALTER TABLE session_manager.sessions ADD COLUMN IF NOT EXISTS container_id BIGINT;
-- CREATE INDEX IF NOT EXISTS idx_sessions_container_id ON session_manager.sessions(container_id);

-- Verify setup
-- \dn+ session_manager
-- \du session_manager_app
