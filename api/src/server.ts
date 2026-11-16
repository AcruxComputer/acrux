import { createFixtureApi } from './app.js'
import { env } from './config.js'
import { serve } from './engines/server/serve.js'
import { createNodeApi } from './node-app.js'

const runtime = env.MODE === 'node' ? createNodeApi() : createFixtureApi()
const service = serve(runtime.app, { host: env.HOST, port: env.PORT, close: async () => runtime.close() })
service.server.keepAliveTimeout = 65000
service.server.headersTimeout = 66000
