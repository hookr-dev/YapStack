import type { FileAttachment } from "./ai";

export function getContentScaleGuidance(transcriptText: string): string {
  const words = transcriptText.split(/\s+/).filter(Boolean).length;
  const segments = (transcriptText.match(/\[seg:/g) || []).length;

  if (words < 150) {
    return `**Content scale:** ~${words} words${segments ? ` across ${segments} segments` : ""}. This is a very short session — capture all topics mentioned.`;
  }

  let ratio: number;
  if (words <= 500) {
    ratio = 0.4;
  } else if (words <= 2000) {
    ratio = 0.3;
  } else {
    ratio = 0.2;
  }

  const target = Math.round(words * ratio);
  return `**Content scale:** ~${words} words${segments ? ` across ${segments} segments` : ""}. Your output should be at most ${target} words. Cover every distinct topic — do not over-condense. Preserve specific names, numbers, decisions, and action items.`;
}

const CITATION_INSTRUCTION = `
When referencing specific parts of the conversation, cite the segment using its ID in this format: [[seg:ID]]. For example: "The team discussed increasing marketing spend [[seg:def456]]." Only cite when it adds value — don't cite every statement.`;

/// Added to the system prompt whenever transcript text is in scope.
/// Transcripts always carry a parenthetical speaker label after the
/// timestamp; this instruction tells the model what each label means and
/// — critically — that statements from other speakers must NOT be
/// attributed to the user. Without this guidance, models routinely
/// summarise things "the user said" when in fact another participant
/// said them on the call.
const SPEAKER_INSTRUCTION = `
Every transcript segment carries a speaker label in parentheses after the timestamp — e.g. \`[seg:abc 0:12] (You) Yes, exactly.\`. The labels mean:
- \`(You)\` — the human running this app. This is the user you are talking to right now. Only statements labelled \`(You)\` are theirs.
- \`(Alice)\`, \`(Bob)\`, \`(Speaker 1)\`, etc. — other people in the recording: people on a call or in the audio the computer played, or people in the room with the user who were picked up by the same microphone. These statements were NOT said by the user.
- \`(Other)\` — another participant whose identity isn't known. Still not the user.

When answering questions about who said what, always attribute to the labelled speaker. Never claim the user said something that another speaker said. If a speaker has only the default label (\`Speaker 1\`, \`Speaker 2\`, …), use that label verbatim — don't invent names.`;

const NOTES_GUIDANCE = `
The user's existing notes are visible later in this prompt — read them before saving to notes. Pick the save_to_notes mode that matches their intent:
- "find_replace" — for changing specific text. Whenever the user asks to change/fix/edit/update/rename a word, phrase, or sentence, this is the right mode. The 'find' string must be an exact substring of the current notes; markdown won't render in the replacement (plain text only).
- "append" — for adding new content (a summary, action items, follow-up notes) below the existing notes.
- "prepend" — for a TL;DR or executive summary at the top.
- "replace" — ONLY when the notes are empty or the user explicitly asks for a full rewrite from scratch. Never use this to make a small change — that's find_replace.

If you're unsure between append and replace, choose append. The user's hand-written notes must be preserved.`;

export const GENERAL_DIRECTIVE = `You are a helpful assistant for a note-taking app. You have access to the user's session content, notes, and session tools.

Answer questions accurately, referencing specific parts of the conversation or notes when relevant. Be concise and direct.

You can use your tools to help the user:
- \`update_title\` — Set a better session title if the user asks
- \`save_to_notes\` — Save content to notes when the user asks you to write, draft, or create something
- \`pin_session\` — Pin/unpin the session
- \`tag_session\` — Add or remove tags from the session
- \`search_folders\` — Search the user's folder tree by name, path, or description. Returns stable folder IDs.
- \`add_session_to_folder\` — Classify the session into an organizational folder by ID. If the available-folders list (when present) shows an obvious match, pass that bracketed ID directly. Otherwise call \`search_folders\` first to get the correct folder_id; never guess an ID or pass a folder name.
- \`search_dictations\` — Search the user's dictation history (a personal knowledge base of short dictated snippets, typically 3–5 sentences each). Useful when the user asks something they may have noted earlier, or when drafting notes that should reflect their prior thinking.
- \`replace_in_transcript\` — Fix a misheard or misspelled word across the current session's transcript. Use ONLY when the user explicitly asks to correct a transcription error (e.g. "replace 'kubernet ease' with 'kubernetes'"). Never edit the transcript pre-emptively. Verify the wrong spelling against the transcript before calling.

Only use tools when the user's request clearly calls for it. For general questions, just answer in text.`;

export function getSystemPrompt(
  directive: string,
  transcriptText: string,
  noteText: string,
  attachments: { name: string; content: string }[],
  options?: { hasSpeakers?: boolean },
): string {
  let context = directive;
  if (transcriptText) {
    context += "\n" + CITATION_INSTRUCTION;
    if (options?.hasSpeakers) {
      context += "\n" + SPEAKER_INSTRUCTION;
    }
  }
  context += "\n" + NOTES_GUIDANCE + "\n\n---\n\n";

  if (transcriptText) {
    context += getContentScaleGuidance(transcriptText) + "\n\n";
    context += `## Session Transcript\n${transcriptText}\n\n`;
  }

  if (noteText) {
    context += `## Notes\n${noteText}\n\n`;
  }

  if (attachments.length > 0) {
    context += `## Attached Files\n`;
    for (const att of attachments) {
      context += `### ${att.name}\n${att.content}\n\n`;
    }
  }

  return context;
}

export interface SessionMeta {
  title: string;
  isPinned: boolean;
  hasNotes: boolean;
}

export function getSystemPromptWithToolContext(
  directive: string,
  transcriptText: string,
  noteText: string,
  attachments: { name: string; content: string }[],
  sessionMeta: SessionMeta,
  folderTreeContext?: string,
  options?: { hasSpeakers?: boolean },
): string {
  const base = getSystemPrompt(
    directive,
    transcriptText,
    noteText,
    attachments,
    options,
  );

  let meta = `\n---\nSession metadata:\n- Current title: "${sessionMeta.title}"\n- Pinned: ${sessionMeta.isPinned ? "yes" : "no"}\n- Has existing notes: ${sessionMeta.hasNotes ? "yes" : "no"}\n`;

  if (folderTreeContext) {
    meta += `\n**Available folders (classify this session before summarizing).** Each line shows \`[folder_id] Name — "description"\`. Pass the bracketed ID directly to \`add_session_to_folder\` when the right folder is obvious from this list; only call \`search_folders\` first if the match is ambiguous or none of these descriptions fit.\n${folderTreeContext}\n`;
  }

  return base + meta;
}

export interface FolderContextLayer {
  name: string;
  description: string;
}

export function getDictationSystemPrompt(
  dictationContext: string,
  attachments: FileAttachment[],
): string {
  let prompt = `You are a helpful assistant for a voice dictation app. The user is viewing their dictation history — a log of past voice-to-text dictations. You can answer questions, search for content across dictations, find patterns, compare entries, and help the user work with their dictation output.

Be concise and direct. Use **bold** for labels. Do not use # or ## markdown headings.

---

## Dictation History

${dictationContext}
`;

  if (attachments.length > 0) {
    prompt += `\n## Attached Files\n`;
    for (const att of attachments) {
      prompt += `### ${att.name}\n${att.content}\n\n`;
    }
  }

  return prompt;
}

export function getMultiSessionSystemPrompt(
  sessionsContext: string,
  attachments: FileAttachment[],
  folderContext?: FolderContextLayer[],
): string {
  let prompt = `You are a helpful assistant for a note-taking app. The user is viewing multiple sessions. You can answer questions, compare sessions, find patterns, and synthesize information across them.

Be concise and direct. Use **bold** for labels when organizing information. Do not use # or ## markdown headings.
${SPEAKER_INSTRUCTION}
---
`;

  if (folderContext && folderContext.length > 0) {
    prompt += `\n**Organizational context:**\n`;
    for (const layer of folderContext) {
      prompt += `- **${layer.name}:** ${layer.description}\n`;
    }
  }

  prompt += `
## Sessions

${sessionsContext}
`;

  if (attachments.length > 0) {
    prompt += `\n## Attached Files\n`;
    for (const att of attachments) {
      prompt += `### ${att.name}\n${att.content}\n\n`;
    }
  }

  return prompt;
}
