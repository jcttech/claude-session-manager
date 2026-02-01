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

-- Verify setup
-- \dn+ session_manager
-- \du session_manager_app
