// new Date().toISOString() embedded mid-prompt in a system template.
const systemPrompt = `You are a coding assistant. Today is ${new Date().toISOString()} and you must be concise.`;
export { systemPrompt };
