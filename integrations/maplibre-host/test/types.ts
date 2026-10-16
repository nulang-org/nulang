import { createMapHost, type MapCommand } from '../index.mjs';
const command: MapCommand = { type: 'layer.visibility', id: 'sites', visible: true };
void command;
// @ts-expect-error layer.visibility requires boolean visible
const invalid: MapCommand = { type: 'layer.visibility', id: 'sites', visible: 'yes' };
void invalid;
void createMapHost;
