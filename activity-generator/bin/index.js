#! /usr/bin/env node

import fs from 'fs';
import { program } from 'commander';
import { select, input, confirm } from '@inquirer/prompts';
import { v4 } from 'uuid';
import path from 'path';
import { parse } from 'json2csv';
import { getFrequency, getAmountInSats, verifyPubKey } from './validation/inputGetters.js';
import { DefaultConfig } from './default_activities_config.js';
import { buildControlNodes, setupControlNodes } from './build_node.js';
import { exec } from "child_process";

program.option('--config <file>');
program.option('--csv-output <file>');
program.parse();

const options = program.opts();
const configFile = options.config;

// Blocking example with fs.readFileSync
const config = configFile ? JSON.parse(fs.readFileSync(configFile, 'utf-8')) : {
    nodes: [],
    activity: []
};
const nodeObj = {};
let controlNodes = config.nodes;

async function init() {
    if (!configFile) {
        await setupControlNodes(config.nodes, nodeObj);
    } else {
        console.log(`Setting up ${config.nodes.length} Controlled Nodes...`)
        controlNodes.forEach(async node => {
            await buildControlNodes({node, nodeObj});
        })
    }
    promptForActivities();
}

init();





let activities = [];
async function promptForActivities() {

    console.log("\nCreate a New Activity");
    console.log("_________________________________\n");
    let activity = {};
    activity.uuid = v4();

    const predefinedActivity = await select({
        message: " \n",
        choices: [
            { name: "Select a predefined activity", value: true },
            { name: "Manually create an activity", value: false }
        ]
    })



    if (predefinedActivity) {
        const selectedPredefinedFreq = await select({
            message: `${DefaultConfig.FREQUENCY_OPTIONS.name} \n`,
            choices: DefaultConfig.FREQUENCY_OPTIONS.options.map((config) => {
                return {
                    name: config.name,
                    value: config.value
                }
            })
        })
        const selectedPredefinedAmount = await select({
            message: `${DefaultConfig.AMOUNT_OPTIONS.name} \n`,
            choices: DefaultConfig.AMOUNT_OPTIONS.options.map((config) => {
                return {
                    name: config.name,
                    value: config.value
                }
            })
        })

        activity.freq = selectedPredefinedFreq
        activity.amt = selectedPredefinedAmount
        activity.action = DefaultConfig.ACTION_TYPE.KEYSEND_PAYMENTS
        activity.src = Object.values(nodeObj)[Math.floor(Math.random() * Object.values(nodeObj).length)].identity_pubkey
        activity.dest = nodeObj[activity.src].possible_dests[Math.floor(Math.random() * nodeObj[activity.src].possible_dests.length)].pub_key
    } else {
        let sourceArray = [{
            name: '(choose random)',
            value: Object.values(nodeObj)[Math.floor(Math.random() * Object.values(nodeObj).length)].identity_pubkey
        }, {
            name: '(input pubkey)',
            value: false
        }]


        activity.src = await select({
            message: "Choose a source? \n",
            choices: sourceArray.concat(Object.keys(nodeObj).map(key => {
                let node = nodeObj[key];
                return {
                    name: `${node.alias || "-----" }:  (${node.identity_pubkey})`,
                    value: node.identity_pubkey
                }
            }))
        })


        if (!activity.src) {
            activity.src = await input({ message: 'Enter pubkey:' });
        }


        let destArray = [{
            name: `(choose random)`,
            value: nodeObj[activity.src].possible_dests[Math.floor(Math.random() * nodeObj[activity.src].possible_dests.length)].pub_key
        }, {
            name: '(input pubkey)',
            value: false
        }]

        activity.dest = await select({
            message: "Choose a destination? \n",
            choices: destArray.concat(nodeObj[activity.src].possible_dests.map(dest => {
                return {
                    name: `${dest.alias || "-----"}:  (${dest.pub_key})`,
                    value: dest.pub_key
                }
            }))

        })

        if (!activity.dest) {
            const singleNodeGraph = Object.values(nodeObj).find((node) => {
                return node.graph.nodes
            }).graph

            const allPossibleNodes = singleNodeGraph.nodes.map((node) => node.pub_key)
            activity.dest = await verifyPubKey(allPossibleNodes)
        }

        activity.action = await input({ message: 'What action?', default: "keysend" });

        activity.frequency = await getFrequency()
        activity.amount = await getAmountInSats()
    }

    activities.push(activity);

    console.log(
        `\n------------------------\nCreated: ${activity.uuid}\nTotal activities: ${activities.length}\n------------------------\n`
    )

    const anotherOne = await confirm({ message: 'Create another one?', default: false });
    if (anotherOne) {
        promptForActivities();
    } else {

        
        if (options.csv) activities = parse(activities, { header: true });
        config.activity = activities;
        runSim = await confirm({ message: 'Run the Simulation?', default: false });
        if (runSim) {
            await exec("echo '\n\n****************\nWe will call the sim-cli from here\n**************'", (error, stdout, stderr) => {
                if (error) {
                    console.log(`error: ${error.message}`);
          
                }
                if (stderr) {
                    console.log(`stderr: ${stderr}`);
            
                }
                console.log(`${stdout}`);
            });

        }
        if (options.csvOutput) fs.writeFileSync(options.csvOutput, parse(activities, { header: true }));
        //save config to original location for ingestion

        fs.writeFileSync(configFile, JSON.stringify(config));
        console.log("Config file created and save to ../config.json");
    }
}