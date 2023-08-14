#! /usr/bin/env node


const LndGrpc = require('lnd-grpc');
const fs = require('fs');
const { program } = require('commander');
program.requiredOption('--config <file>');
program.parse();


import { input } from '@inquirer/prompts';
const options = program.opts();
const configFile = options.config;

// Blocking example with fs.readFileSync
const fileName = configFile;
const config = JSON.parse(fs.readFileSync(fileName, 'utf-8')).nodes[0];
let nodeObj = {};

const grpc = new LndGrpc({
    host: config.ip,
    cert: config.cert,
    macaroon: config.macaroon,
})


grpc.connect();





(async function() {
    console.log(grpc.state)
    const { WalletUnlocker, Lightning } = grpc.services
    // Do something cool if we detect that the wallet is locked.
    grpc.on(`connected`, () => console.log('wallet connected!'))

    // Do something cool if we detect that the wallet is locked.
    grpc.on(`locked`, async () => {
        await grpc.activateLightning()

        console.log(grpc.state) // active
    })

    // Do something cool when the wallet gets unlocked.
    grpc.on(`active`, async () => {
        console.log('wallet unlocked!')
        const current_node = await Lightning.getInfo();
        nodeObj[current_node.identity_pubkey] = current_node;
        //dump graph information
        const nodeGraph = await Lightning.describeGraph()
        nodeObj[current_node.identity_pubkey].graph = nodeGraph;

        //create array of possible destintations for node
        nodeObj[current_node.identity_pubkey].possible_dests = nodeGraph.nodes.filter((n) => {
            return n.pub_key != current_node.identity_pubkey
        })

        console.log(nodeObj[current_node.identity_pubkey].possible_dests)

        grpc.disconnect()
    })
    // Do something cool when the connection gets disconnected.
    grpc.on(`disconnected`, () => {
      if(Object.keys(nodeObj).length == config.nodes.length) promptForActivities();
    })


})()



async function promptForActivities() {
        console.log(nodeObj);
        //Create activity descriptions through prompt
        //TODO: Make it more flexible for multiple nodes

        //Choose src from nodes you control
        const srcAnswer = await inquirer.select({
            message: "Choose a src?",
            choices: config.nodes.map(node => {
                return {
                    name: `${node.alias}(${node.info.identity_pubkey})`,
                    value: node.info.identity_pubkey
                }
            })
        })




        const destAnswer = await inquirer.select({
            message: "Choose a destination?",
            choices: config.nodes.map(node => {
                return {
                    name: `${node.alias}(${node.info.identity_pubkey})`,
                    value: node.info.identity_pubkey
                }
            })

        })

        const questions = [{
            type: 'input',
            name: 'name',
            message: "What's the first destination?",
        }, ];

        inquirer.prompt(questions).then(answers => {
            console.log(`Hi ${answers.name}!`);
        });


    }