"""Negative: database utility without any LLM imports."""
import sqlite3

def query(db_path: str, sql: str) -> list:
    conn = sqlite3.connect(db_path)
    cursor = conn.execute(sql)
    return cursor.fetchall()
