CREATE TABLE clicks (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    code       TEXT NOT NULL,
    clicked_at TEXT NOT NULL DEFAULT (datetime('now')),
    user_agent TEXT,
    ip         TEXT,
    FOREIGN KEY (code) REFERENCES links(code) ON DELETE CASCADE
);