// Turns the probe's results into the numbers the comparison document quotes.
const r = require(require('path').join(require('./paths.js').OUT, 'results.json'));

const byArea = {};
for (const x of r) {
  byArea[x.area] = byArea[x.area] || { total: 0, same: 0, wrong: 0, refused: 0, both: 0, accepted: 0, ours: 0, ids: [] };
  const a = byArea[x.area];
  a.total += 1;
  if (x.verdict === 'same') a.same += 1;
  else if (x.verdict === 'wrong-answer') { a.wrong += 1; a.ids.push('W ' + x.id); }
  else if (x.verdict === 'refused') { a.refused += 1; a.ids.push('R ' + x.id); }
  else if (x.verdict === 'both-refuse-differently') { a.both += 1; a.ids.push('B ' + x.id); }
  else if (x.verdict === 'accepted') { a.accepted += 1; a.ids.push('A ' + x.id); }
  else if (x.verdict === 'ours-only') a.ours += 1;
}
console.log('| area | cases | agree | differ | refused here | both refuse | accepted here |');
console.log('|---|---|---|---|---|---|---|');
for (const [area, a] of Object.entries(byArea).sort()) {
  console.log(`| ${area} | ${a.total} | ${a.same + a.ours} | ${a.wrong} | ${a.refused} | ${a.both} | ${a.accepted} |`);
}
const t = r.reduce((acc, x) => { acc[x.verdict] = (acc[x.verdict] || 0) + 1; return acc; }, {});
console.log('\ntotals', JSON.stringify(t), 'cases', r.length);
