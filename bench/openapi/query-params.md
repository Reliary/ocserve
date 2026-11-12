# Declared query parameters vs handler reads

| route | parameter(s) | verdict |
|---|---|---|
| `/api/agent` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/agent/{agentID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/command` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/config` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/event` | types | cited D-EVENT-TYPE-FILTER |
| `/api/experimental/artifacts` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/experimental/artifacts/providers` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/experimental/artifacts/{artifactID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/experimental/client-mcp` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/experimental/client-mcp` | ticket | cited D-CLIENT-MCP-TICKET |
| `/api/experimental/persistent-pty/{ptyID}/connect` | cursor | cited D-PTY-RESUME-CURSOR |
| `/api/experimental/persistent-pty/{ptyID}/connect` | role | cited D-PERSISTENT-PTY-PARAMS |
| `/api/experimental/persistent-pty/{ptyID}/connect` | attachment_id | cited D-PERSISTENT-PTY-PARAMS |
| `/api/experimental/persistent-pty/{ptyID}/connect` | takeover | cited D-PERSISTENT-PTY-PARAMS |
| `/api/experimental/persistent-pty/{ptyID}/connect` | input_protocol | cited D-PERSISTENT-PTY-PARAMS |
| `/api/experimental/persistent-pty/{ptyID}/connect` | ticket | cited D-PERSISTENT-PTY-PARAMS |
| `/api/experimental/schedule` | sessionID | read |
| `/api/experimental/session/stats` | from | cited D-STATS-WINDOW |
| `/api/experimental/session/stats` | to | cited D-STATS-WINDOW |
| `/api/experimental/session/stats` | project | cited D-STATS-WINDOW |
| `/api/experimental/session/stats` | timezone | cited D-STATS-WINDOW |
| `/api/experimental/session/stats` | tools | cited D-STATS-WINDOW |
| `/api/experimental/session/{sessionID}/export` | sanitize | cited D-EXPORT-SANITIZE |
| `/api/experimental/session/{sessionID}/log` | after,follow | read |
| `/api/experimental/session/{sessionID}/terminal/read` | lines | cited D-TERMINAL-READ-LINES |
| `/api/experimental/usage/limits` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/form` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/fs/find` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/fs/list` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/integration` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/integration/{integrationID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/integration/{integrationID}/connect/command/{attemptID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/integration/{integrationID}/connect/oauth/{attemptID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/location` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/mcp` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/mcp/resource` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/model` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/model` | types | cited D-MODEL-TYPE-FILTER |
| `/api/model/default` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/permission/request` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/permission/saved` | projectID | read |
| `/api/plugin` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/plugin/gui` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/project` | archived | cited D-PROJECT-ARCHIVE |
| `/api/provider` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/provider/{providerID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/pty` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/pty/{ptyID}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/pty/{ptyID}/connect` | location[directory] | cited D-PTY-TICKET-QUERY |
| `/api/pty/{ptyID}/connect` | cursor | cited D-PTY-RESUME-CURSOR |
| `/api/pty/{ptyID}/connect` | ticket | cited D-PTY-TICKET-QUERY |
| `/api/reference` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/session` | limit,order,search,parentID,archived,directory,project,subpath,cursor | read |
| `/api/session/{sessionID}/diff` | from | cited D-SESSION-DIFF-WINDOW |
| `/api/session/{sessionID}/diff` | to | cited D-SESSION-DIFF-WINDOW |
| `/api/session/{sessionID}/diff` | context | cited D-SESSION-DIFF-WINDOW |
| `/api/session/{sessionID}/message` | limit,order,cursor,type | read |
| `/api/shell` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/shell/{id}` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/shell/{id}/output` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/shell/{id}/output` | cursor | cited D-SHELL-OUTPUT-PAGING |
| `/api/shell/{id}/output` | limit | cited D-SHELL-OUTPUT-PAGING |
| `/api/skill` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/vcs` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/vcs/base` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/vcs/branch` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/vcs/diff` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/vcs/status` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/websearch/provider` | location | cited D-LOCATION-SINGLE-SERVER |
| `/api/worktree` | projectID | cited D-WORKTREE-PROJECT-SCOPE |
