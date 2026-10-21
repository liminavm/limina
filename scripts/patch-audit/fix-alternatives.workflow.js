export const meta = {
  name: 'fix-alternatives',
  description: 'Blind proposers write alternative fixes for one upstream bug; a judge ranks them with ours unlabelled; skeptics try to break the top two',
  whenToUse: 'docs/upstreaming/patch-diligence.md §5, before the §6 test matrix',
  phases: [
    { title: 'Propose', detail: 'one independent proposer per lens, none sees our fix' },
    { title: 'Judge', detail: 'all candidates under neutral labels, ours among them' },
    { title: 'Refute', detail: 'one skeptic per top-two candidate' },
  ],
}

// args:
//   brief       path to the bug brief: mechanism, paths that reach it, the broken invariant,
//               reproducer and verdicts. It must not contain or point at our fix.
//   tree        path to a checkout of upstream at `sha` holding no other refs
//   sha         the upstream commit the brief and the tree describe
//   history     optional path to a full-history clone, for the maintainer-idiom lens
//   ours        array of { name, diff }: our fix(es), diff text inline; never shown to proposers
//   extraLenses optional array of { key, prompt } appended to LENSES (rules learned from
//               maintainer rewrites live in patch-diligence.md §5 until they earn a lens)

const LENSES = [
  {
    key: 'invariant',
    prompt: 'Name the invariant that breaks, and the single place where restoring it means no ' +
      'other code has to know about the bug. When several stages consume one decision and ' +
      'disagree, fix the decision, not each consumer.',
  },
  {
    key: 'maintainer',
    prompt: 'Read the recent commits to the touched files and how their authors fixed similar ' +
      'bugs in this subsystem. Propose the fix those authors would write, in their idiom.',
  },
  {
    key: 'smallest',
    prompt: 'Propose the smallest correct diff. Prefer removing state, conditions or calls over ' +
      'adding them.',
  },
  {
    key: 'mechanism',
    prompt: 'Find the mechanism the subsystem or platform already provides for this exact ' +
      'problem (a save/restore helper, a guard flag, an existing ordering rule) and use it.',
  },
]

const CANDIDATE = {
  type: 'object',
  properties: {
    summary: { type: 'string', description: 'what the fix does, two sentences' },
    cause: { type: 'string', description: 'the root cause as this fix understands it' },
    diff: { type: 'string', description: 'unified diff against the tree' },
    changes_besides_bug: { type: 'string' },
    risks: { type: 'string', description: 'paths it could break, races, leaks' },
  },
  required: ['summary', 'cause', 'diff', 'changes_besides_bug', 'risks'],
}

const RANKING = {
  type: 'object',
  properties: {
    ranking: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          label: { type: 'string' },
          fixes_cause: { type: 'string', description: 'cause or consequences, and why' },
          places_that_must_agree: { type: 'integer' },
          changes_besides_bug: { type: 'string' },
          reads_like_surrounding_code: { type: 'string' },
          diff_lines: { type: 'integer' },
          correct: { type: 'boolean', description: 'fixes the bug on every path in the brief' },
          verdict: { type: 'string' },
        },
        required: ['label', 'fixes_cause', 'places_that_must_agree', 'changes_besides_bug',
          'reads_like_surrounding_code', 'diff_lines', 'correct', 'verdict'],
      },
    },
    same_shape: {
      type: 'array',
      items: { type: 'array', items: { type: 'string' } },
      description: 'groups of labels that are the same fix in different words',
    },
  },
  required: ['ranking', 'same_shape'],
}

const REFUTATION = {
  type: 'object',
  properties: {
    broken: { type: 'boolean' },
    path: { type: 'string', description: 'the concrete call path or input where it fails, or why none exists' },
    test: { type: 'string', description: 'a reproducer mode that would show it' },
  },
  required: ['broken', 'path', 'test'],
}

const BLIND =
  `Read the bug brief at ${args.brief}. Work only from that brief and the source tree at ` +
  `${args.tree} (upstream at ${args.sha}). Do not read any other directory, do not list or check ` +
  'out other git refs, and do not search the web or issue trackers for a fix to this bug: the ' +
  'point is an independent proposal. Read every path that reaches the faulty code before ' +
  'proposing. Make no changes to the tree; return the diff.' +
  (args.history ? ` For history older than the tree holds, use ${args.history}, read-only.` : '')

const lenses = LENSES.concat(args.extraLenses || [])

phase('Propose')
const proposed = await parallel(lenses.map(l => () =>
  agent(`${BLIND}\n\nLens: ${l.prompt}`, { label: `propose:${l.key}`, phase: 'Propose', schema: CANDIDATE })
    .then(c => c && { origin: `lens:${l.key}`, ...c })))

const pool = proposed.filter(Boolean).concat((args.ours || []).map(o => ({
  origin: `ours:${o.name}`, summary: '', cause: '', diff: o.diff, changes_besides_bug: '', risks: '',
})))
log(`${pool.length} candidates (${proposed.filter(Boolean).length}/${lenses.length} proposers returned)`)

// Neutral labels in an order set by the diff text, so the judge cannot tell ours by position.
const hash = s => [...s].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7)
const labelled = pool
  .map(c => ({ ...c, key: hash(c.diff) }))
  .sort((a, b) => a.key - b.key)
  .map((c, i) => ({ ...c, label: String.fromCharCode(65 + i) }))

const shown = labelled.map(c => `## Candidate ${c.label}\n\`\`\`diff\n${c.diff}\n\`\`\``).join('\n\n')

phase('Judge')
const judged = await agent(
  `Read the bug brief at ${args.brief} and the source tree at ${args.tree} (upstream at ` +
  `${args.sha}); do not read other directories or refs. Below are candidate fixes under neutral ` +
  'labels. Judge each on: whether it fixes the cause or patches its consequences; how many ' +
  'places must stay in agreement afterwards; what behaviour it changes besides the bug; whether ' +
  'it reads like the surrounding code; its size; and whether it is correct on every path in the ' +
  'brief. Rank best first and group candidates that are the same fix.\n\n' + shown,
  { label: 'judge', phase: 'Judge', schema: RANKING })

const top = judged ? judged.ranking.filter(r => r.correct).slice(0, 2) : []
if (judged && top.length < 2) log(`only ${top.length} candidate(s) judged correct; refuting those`)

phase('Refute')
const refuted = await parallel(top.map(r => () => {
  const c = labelled.find(x => x.label === r.label)
  return agent(
    `Read the bug brief at ${args.brief} and the source tree at ${args.tree} (upstream at ` +
    `${args.sha}); do not read other directories or refs. Try to break this fix: find a call ` +
    'path, ordering or input where it leaves the bug in place or introduces a new one. Default ' +
    'to broken=false only when you have read every caller and found none.\n\n' +
    `\`\`\`diff\n${c.diff}\n\`\`\``,
    { label: `refute:${r.label}`, phase: 'Refute', schema: REFUTATION })
    .then(v => v && { label: r.label, ...v })
}))

return {
  candidates: labelled.map(({ key, ...c }) => c),
  judged,
  refuted: refuted.filter(Boolean),
}
