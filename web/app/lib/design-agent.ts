import { reviewProgram, reviewWorkflow } from "./fixtures";
import type { ChatMessage, ConversationSummary } from "./protocol";
import { sendChat, subscribeToChat } from "./runtime-client";

/**
 * The workflow builder the operator talks to.
 *
 * Drafting is a conversation, not a request/response: the EA may ask a
 * question, explain itself, or propose a program, and a proposal may never
 * come. So messages arrive on a subscription rather than resolving a promise.
 */
export type DesignAgent = {
  send(text: string, selection?: string[]): Promise<void>;
  subscribe(
    onMessage: (message: ChatMessage) => void,
    onConnectionChange: (connected: boolean) => void,
    onConversation?: (conversation: ConversationSummary) => void,
  ): () => void;
};

/** Backed by the real EA through `omar serve`. */
export function eaDesignAgent(serveUrl: string): DesignAgent {
  let conversationId: string | undefined;
  return {
    send: (text, selection) => sendChat(serveUrl, text, selection ?? [], undefined, conversationId),
    subscribe: (onMessage, onConnectionChange, onConversation) =>
      subscribeToChat(serveUrl, onMessage, onConnectionChange, (conversation) => {
        conversationId = conversation.id;
        onConversation?.(conversation);
      }),
  };
}

/**
 * Offline stand-in for demo mode and CI.
 *
 * It answers on the same interface the EA does, so the studio's conversation
 * and confirmation logic is the code that ships rather than a test-only path.
 * It always proposes the same program — a fixture, not a drafting model.
 */
export function scriptedDesignAgent(): DesignAgent {
  let sequence = 0;
  const listeners = new Set<(message: ChatMessage) => void>();

  const emit = (message: Omit<ChatMessage, "sequence">) => {
    sequence += 1;
    for (const listener of listeners) listener({ ...message, sequence });
  };

  return {
    async send(text: string) {
      emit({ role: "operator", text, progress: false, design: null, selection: [] });
      // Asynchronous, like a real reply, so the studio cannot accidentally
      // depend on drafting being synchronous.
      setTimeout(() => {
        emit({
          role: "assistant",
          progress: false,
          selection: [],
          text: "Drafted a three-reaction review loop: planner drafts a plan, reviewer critiques it, planner integrates the critique into the result.",
          design: {
            program: reviewProgram,
            inputs: { request: text },
            preview: {
              ...reviewWorkflow,
              status: "ready",
              sequence: 0,
              current_tag: null,
              ports: reviewWorkflow.ports.map((port) => ({
                ...port,
                value: port.name === "request" ? text : null,
                last_tag: null,
              })),
              reactions: reviewWorkflow.reactions.map((reaction) => ({
                ...reaction,
                status: "idle" as const,
                invocation_id: null,
              })),
            },
          },
        });
      }, 30);
    },
    subscribe(onMessage) {
      listeners.add(onMessage);
      return () => listeners.delete(onMessage);
    },
  };
}
