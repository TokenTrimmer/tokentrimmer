import type {
	IDataObject,
	IExecuteFunctions,
	INodeExecutionData,
	INodeType,
	INodeTypeDescription,
	JsonObject,
} from 'n8n-workflow';
import { NodeApiError, NodeConnectionTypes, NodeOperationError } from 'n8n-workflow';

import type { CostInfo } from './GenericFunctions';
import {
	buildMessages,
	buildRequestBody,
	buildTtHeaders,
	chatCompletionsUrl,
	parseCostInfo,
} from './GenericFunctions';

/**
 * NodeApiError enriched with cost/trace info parsed from the FAILED
 * response's headers — `x-tokentrimmer-trace-id` is present on every gateway
 * response including errors (API ref §6.2), and cost headers exist on
 * post-dispatch failures, so continueOnFail items stay correlatable.
 */
interface TtNodeApiError extends NodeApiError {
	ttCostInfo?: CostInfo;
}

interface ChatOptions {
	tag?: string;
	costLimitUsd?: number;
	cacheOverride?: string;
	maxTokens?: number;
	temperature?: number;
}

export class TokenTrimmerChat implements INodeType {
	description: INodeTypeDescription = {
		displayName: 'TokenTrimmer Chat',
		name: 'tokenTrimmerChat',
		icon: 'file:tokentrimmer.svg',
		group: ['transform'],
		version: 1,
		subtitle: '={{$parameter["model"]}}',
		description:
			'Send a chat completion through the TokenTrimmer Gateway and surface per-call cost and savings',
		defaults: {
			name: 'TokenTrimmer Chat',
		},
		inputs: [NodeConnectionTypes.Main],
		outputs: [NodeConnectionTypes.Main],
		credentials: [
			{
				name: 'tokenTrimmerApi',
				required: true,
			},
		],
		properties: [
			{
				displayName: 'Model',
				name: 'model',
				type: 'string',
				required: true,
				default: '',
				placeholder: 'claude-haiku-4-5',
				description:
					'Model ID to request. Bare IDs are accepted; use the &lt;provider&gt;/&lt;model&gt; convention (e.g. anthropic/claude-haiku-4-5) to disambiguate (API ref §1). A routing rule may rewrite this — the response model and costInfo.modelUsed reflect what actually ran.',
			},
			{
				displayName: 'Input Mode',
				name: 'inputMode',
				type: 'options',
				options: [
					{
						name: 'Prompt',
						value: 'prompt',
						description: 'A single user prompt with an optional system prompt',
					},
					{
						name: 'Messages (JSON)',
						value: 'messagesJson',
						description: 'A raw OpenAI-shaped messages array',
					},
				],
				default: 'prompt',
			},
			{
				displayName: 'Prompt',
				name: 'prompt',
				type: 'string',
				typeOptions: { rows: 4 },
				default: '',
				placeholder: 'e.g. {{ $json.chatInput }}',
				displayOptions: {
					show: {
						inputMode: ['prompt'],
					},
				},
				description: 'The user message to send',
			},
			{
				displayName: 'System Prompt',
				name: 'systemPrompt',
				type: 'string',
				typeOptions: { rows: 2 },
				default: '',
				displayOptions: {
					show: {
						inputMode: ['prompt'],
					},
				},
				description: 'Optional system message, sent before the prompt',
			},
			{
				displayName: 'Messages (JSON)',
				name: 'messagesJson',
				type: 'json',
				default: '[\n  {"role": "user", "content": "Hello"}\n]',
				displayOptions: {
					show: {
						inputMode: ['messagesJson'],
					},
				},
				description: 'Raw OpenAI messages array, passed through untouched',
			},
			{
				displayName: 'Options',
				name: 'options',
				type: 'collection',
				placeholder: 'Add Option',
				default: {},
				options: [
					{
						displayName: 'Cache Override',
						name: 'cacheOverride',
						type: 'options',
						options: [
							{ name: 'Bypass', value: 'bypass' },
							{ name: 'Disabled', value: 'disabled' },
							{ name: 'Force Write', value: 'force-write' },
							{ name: 'Read Only', value: 'read-only' },
						],
						default: 'bypass',
						description: 'Override cache behavior for this request (X-TokenTrimmer-Cache)',
					},
					{
						displayName: 'Cost Limit (USD)',
						name: 'costLimitUsd',
						type: 'number',
						default: 0.05,
						description:
							'Sent as X-TokenTrimmer-Cost-Limit-Usd — the gateway rejects with 402 if the estimated cost exceeds this',
					},
					{
						displayName: 'Max Tokens',
						name: 'maxTokens',
						type: 'number',
						default: 1024,
						description: 'Maximum completion tokens (request body max_tokens)',
					},
					{
						displayName: 'Tag',
						name: 'tag',
						type: 'string',
						default: '',
						placeholder: 'feature=my-workflow',
						description:
							'Free-form cost-attribution tag, sent as X-TokenTrimmer-Tag (drives per-feature dashboard attribution on hosted)',
					},
					{
						displayName: 'Temperature',
						name: 'temperature',
						type: 'number',
						default: 1,
						description: 'Sampling temperature (request body temperature)',
					},
				],
			},
		],
	};

	async execute(this: IExecuteFunctions): Promise<INodeExecutionData[][]> {
		const items = this.getInputData();
		const returnData: INodeExecutionData[] = [];

		for (let i = 0; i < items.length; i++) {
			try {
				const model = this.getNodeParameter('model', i) as string;
				const inputMode = this.getNodeParameter('inputMode', i) as 'prompt' | 'messagesJson';
				const options = this.getNodeParameter('options', i, {}) as ChatOptions;
				const credentials = await this.getCredentials('tokenTrimmerApi');

				let url: string;
				let body: object;
				let ttHeaders: Record<string, string>;
				try {
					const messages = buildMessages({
						mode: inputMode,
						prompt:
							inputMode === 'prompt'
								? (this.getNodeParameter('prompt', i, '') as string)
								: undefined,
						systemPrompt:
							inputMode === 'prompt'
								? (this.getNodeParameter('systemPrompt', i, '') as string)
								: undefined,
						// No string cast: a json-type parameter comes back as a real
						// array/object when a pure expression resolves to one —
						// buildMessages accepts both shapes.
						messagesJson:
							inputMode === 'messagesJson'
								? (this.getNodeParameter('messagesJson', i) as unknown)
								: undefined,
					});
					url = chatCompletionsUrl(credentials.baseUrl as string);
					body = buildRequestBody({
						model,
						messages,
						maxTokens: options.maxTokens,
						temperature: options.temperature,
					});
					ttHeaders = buildTtHeaders({
						tag: options.tag,
						costLimitUsd: options.costLimitUsd,
						cacheOverride: options.cacheOverride,
					});
				} catch (error) {
					throw new NodeOperationError(this.getNode(), error as Error, { itemIndex: i });
				}

				let response;
				try {
					response = await this.helpers.httpRequestWithAuthentication.call(
						this,
						'tokenTrimmerApi',
						{
							method: 'POST',
							url,
							body,
							headers: ttHeaders,
							json: true,
							returnFullResponse: true,
						},
					);
				} catch (error) {
					const apiError = new NodeApiError(this.getNode(), error as JsonObject, {
						itemIndex: i,
					}) as TtNodeApiError;
					// Preserve the failed response's x-tokentrimmer-* headers (trace
					// id always; cost headers on post-dispatch failures) so the
					// continueOnFail path can surface them.
					const failedHeaders = (error as { response?: { headers?: unknown } } | null)
						?.response?.headers;
					if (failedHeaders !== null && typeof failedHeaders === 'object') {
						apiError.ttCostInfo = parseCostInfo(failedHeaders as Record<string, unknown>);
					}
					throw apiError;
				}

				// costInfo is a top-level sibling of the completion fields on every
				// successful execution — saved USD visible in the n8n output panel.
				returnData.push({
					json: {
						...(response.body as IDataObject),
						costInfo: parseCostInfo(
							(response.headers ?? {}) as Record<string, unknown>,
						) as unknown as IDataObject,
					},
					pairedItem: { item: i },
				});
			} catch (error) {
				if (this.continueOnFail()) {
					const json: IDataObject = {
						error: error instanceof Error ? error.message : String(error),
					};
					// HTTP failures carry the error response's parsed headers —
					// costInfo.traceId correlates the failed item against gateway
					// logs (and cost fields are set on post-dispatch failures).
					const failedCostInfo = (error as TtNodeApiError).ttCostInfo;
					if (failedCostInfo !== undefined) {
						json.costInfo = failedCostInfo as unknown as IDataObject;
					}
					returnData.push({ json, pairedItem: { item: i } });
					continue;
				}
				throw error;
			}
		}

		return [returnData];
	}
}
