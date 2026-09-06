"use client";

import { useEffect, useRef, useState } from "react";
import type { ConversationSummary } from "./lib/protocol";
import { fetchConversations, selectConversation } from "./lib/runtime-client";

export function ChatHistory({ serveUrl, busy, onClose, onSelect }: {
  serveUrl: string;
  busy: boolean;
  onClose: () => void;
  onSelect: (conversation: ConversationSummary) => void;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [chats, setChats] = useState<ConversationSummary[]>([]);
  const [activeId, setActiveId] = useState("");
  const [loading, setLoading] = useState(true);
  const [switching, setSwitching] = useState(false);
  const [error, setError] = useState("");
  const [query, setQuery] = useState("");

  useEffect(() => {
    const dialog = dialogRef.current;
    const opener = document.activeElement as HTMLElement | null;
    dialog?.showModal();
    const abort = new AbortController();
    void fetchConversations(serveUrl, abort.signal).then((history) => {
      setChats(history.conversations);
      setActiveId(history.active_id);
      setLoading(false);
    }).catch((cause) => {
      if (abort.signal.aborted) return;
      setError(cause instanceof Error ? cause.message : String(cause));
      setLoading(false);
    });
    return () => {
      abort.abort();
      dialog?.close();
      opener?.focus();
    };
  }, [serveUrl]);

  async function select(id?: string) {
    if (id === activeId) {
      onClose();
      return;
    }
    setSwitching(true);
    setError("");
    try {
      onSelect(await selectConversation(serveUrl, id));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setSwitching(false);
    }
  }

  const visible = chats.filter((chat) => chat.title.toLowerCase().includes(query.toLowerCase()));
  return (
    <dialog ref={dialogRef} className="chat-history" aria-labelledby="chat-history-title" onCancel={onClose}>
      <header>
        <h2 id="chat-history-title">Chat history</h2>
        <button type="button" onClick={onClose} aria-label="Close chat history">×</button>
      </header>
      <p>Conversations are saved automatically on this runtime.</p>
      <div className="history-actions">
        <input aria-label="Search chats" placeholder="Search chats…" value={query} onChange={(event) => setQuery(event.target.value)} />
        <button type="button" disabled={busy || loading || switching} onClick={() => void select()}>New chat</button>
      </div>
      {busy ? <p role="status">Wait for the current reply or run to finish before switching chats.</p> : null}
      {error ? <p role="alert" className="history-error">{error}</p> : null}
      <ul aria-label="Saved chats" aria-busy={loading || switching}>
        {visible.map((chat) => (
          <li key={chat.id}>
            <button type="button" disabled={switching || (busy && chat.id !== activeId)} aria-current={chat.id === activeId ? "true" : undefined} onClick={() => void select(chat.id)}>
              <span>{chat.title}</span>
              <small>{chat.message_count} messages · {new Date(chat.updated_at).toLocaleString()}{chat.id === activeId ? " · Current" : ""}</small>
            </button>
          </li>
        ))}
      </ul>
      {loading ? <p role="status">Loading conversations…</p> : visible.length === 0 && !error ? <p>No chats found.</p> : null}
    </dialog>
  );
}
