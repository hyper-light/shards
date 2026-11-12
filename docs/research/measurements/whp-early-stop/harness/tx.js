// Prints a Claude Code transcript's tool calls and results, lines FROM..TO (1-based), each
// result cut to MAX characters, optionally only those matching RE:
//   node tx.js FILE FROM TO MAX [RE]
const fs = require('fs');
const [file, from, to, max, re] = process.argv.slice(2);
const lines = fs.readFileSync(file, 'utf8').split('\n');
const pattern = re ? new RegExp(re) : null;
for (let i = Number(from) - 1; i < Math.min(Number(to), lines.length); i++) {
  if (!lines[i]) continue;
  let j;
  try { j = JSON.parse(lines[i]); } catch (e) { continue; }
  const content = j.message && j.message.content;
  if (!Array.isArray(content)) continue;
  for (const c of content) {
    let text = null;
    if (c.type === 'tool_use') text = 'TOOL_USE ' + JSON.stringify(c.input);
    else if (c.type === 'tool_result') {
      const parts = Array.isArray(c.content) ? c.content.map(p => p.text || '').join('\n') : String(c.content);
      text = 'TOOL_RESULT ' + parts;
    } else if (c.type === 'text') text = 'TEXT ' + c.text;
    if (text === null) continue;
    if (pattern && !pattern.test(text)) continue;
    console.log(`=== line ${i + 1} ${j.timestamp || ''}`);
    console.log(text.length > Number(max) ? text.slice(0, Number(max)) + ' [...]' : text);
  }
}
