import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const request = JSON.parse(readFileSync(0, 'utf8'))
if (request.version !== 1) throw new Error('Unsupported projection protocol')
const contract = JSON.parse(
  readFileSync(join(request.revisionRoot, 'schemas/contract.json'), 'utf8'),
)

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical)
  if (value !== null && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value).sort().map(key => [key, canonical(value[key])]),
    )
  }
  return value
}

const artifacts = {}
for (const [consumer, definition] of Object.entries(contract.consumers)) {
  const content = JSON.stringify(canonical(definition.runtime))
  artifacts[`apps/${consumer}/generated/contract.js`] = createHash('sha256')
    .update(content)
    .digest('hex')
}
process.stdout.write(JSON.stringify({ version: 1, artifacts }) + '\n')
