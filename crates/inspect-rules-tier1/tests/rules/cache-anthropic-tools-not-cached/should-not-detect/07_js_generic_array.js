// A generic config file, nothing to do with LLMs.
const buildTools = ["eslint", "prettier", "tsc"];
function run(list) { return list.map((t) => t.trim()); }
module.exports = { buildTools, run };
