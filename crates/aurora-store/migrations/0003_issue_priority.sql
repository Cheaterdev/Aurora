ALTER TABLE issue ADD COLUMN priority TEXT
    CHECK (priority IN ('urgent', 'high', 'medium', 'low'));
