#! /usr/bin/env node


import LndGrpc from 'lnd-grpc';
import fs from 'fs';
import { program } from 'commander';
import { select, input, confirm } from '@inquirer/prompts';
import { v4 } from 'uuid';
program.requiredOption('--config <file>');
program.parse();

const options = program.opts();
const configFile = options.config;

// Blocking example with fs.readFileSync
const fileName = configFile;
const config = JSON.parse(fs.readFileSync(fileName, 'utf-8'));
let nodeObj = {};
const controlNodes = config.nodes;



console.log('Setting up Control Nodes...')
async function buildControlNodes() {
    if (!controlNodes.length) return promptForActivities();

    let node = controlNodes.shift()
    const grpc = new LndGrpc({
        host: node.ip,
        cert: node.cert,
        macaroon: node.macaroon,
    })

    grpc.connect();
    (async function() {

        const { WalletUnlocker, Lightning } = grpc.services
        // Do something cool if we detect that the wallet is locked.
        grpc.on(`connected`, () => console.log('wallet connected!'))
        // Do something cool if we detect that the wallet is locked.
        grpc.on(`locked`, async () => {
            await grpc.activateLightning()
        })

        // Do something cool when the wallet gets unlocked.
        grpc.on(`active`, async () => {

            const current_node = await Lightning.getInfo();
            nodeObj[current_node.identity_pubkey] = current_node;
            //dump graph information
            const nodeGraph = await Lightning.describeGraph()
            nodeObj[current_node.identity_pubkey].graph = nodeGraph;

            //create array of possible destintations for node
            nodeObj[current_node.identity_pubkey].possible_dests = nodeGraph.nodes.filter((n) => {
                return n.pub_key != current_node.identity_pubkey
            })
            grpc.disconnect()
        })
        // Do something cool when the connection gets disconnected.
        grpc.on(`disconnected`, () => {
            if (Object.keys(nodeObj).length == config.nodes.length) promptForActivities();
            else buildControlNodes();
        })


    })()

}

buildControlNodes();

let activities = [];
async function promptForActivities() {
  
    console.log("\nCreate a New Activity");
    console.log("***************************************************\n");
    let activity = {};
    activity.uuid = v4();

    activity.src = await select({
        message: "Choose a source? \n",
        choices: Object.keys(nodeObj).map(key => {
            let node = nodeObj[key];
            return {
                name: `${node.alias}:  (${node.identity_pubkey})`,
                value: node.identity_pubkey
            }
        })
    })

    activity.dest = await select({
        message: "Choose a destination? \n",
        choices: nodeObj[activity.src].possible_dests.map(dest => {
            return {
                name: `${dest.alias}:  (${dest.pub_key})`,
                value: dest.pub_key
            }
        })

    })

    activity.action = await input({ message: 'What action?', default: "keysend" });
    activity.frequency = await input({ message: 'At what time would you like to run this action?', default: 0 });
    activity.frequency = parseInt(activity.frequency);
    let amount = await input({ message: 'How many sats?', default: 1000 });
    activity.amount = parseInt(amount);
    activities.push(activity);

    const anotherOne = await confirm({ message: 'Create another one?', default: false });
    console.log("------------------------");
    console.log(`Created: ${activity.uuid}\nTotal activities: ${activities.length}`);
    console.log("------------------------");
    if (anotherOne) {
        promptForActivities();
    } else {
        console.log(activities);
    }
}