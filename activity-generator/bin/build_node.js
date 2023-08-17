#! /usr/bin/env node

import LndGrpc from 'lnd-grpc';
import chalk from "chalk"
import { input, confirm } from '@inquirer/prompts';

async function setupControlNodes(controlNodes, nodeObj) {
  console.log("\nAdd a Control Node");
  console.log("_________________________________\n");

  let control_node = {
    ip: "",
    macaroon: "",
    cert: "",
  }
  
  control_node.ip = await input({ message: 'Enter the node host address (localhost:10001)'});
  control_node.macaroon = await input({ message: 'Enter the node macaroon path'});
  control_node.cert = await input({ message: 'Enter the node cert file path'});

  try {
    const validatedNode = await buildControlNodes({node: control_node, nodeObj})
    controlNodes.push(validatedNode)
    console.log("_________________________________\n")
    console.log(chalk.green(`\n ${validatedNode.alias} node connected successfully \n`))
    console.log("_________________________________\n")
    const anotherOne = await confirm({ message: 'Build another control node?', default: false });
    if (anotherOne) { 
      await setupControlNodes(controlNodes, nodeObj) 
    }

  } catch (err) {
    console.log("_________________________________\n")
    err?.code && console.log(chalk.red(`Error code: ${err.code}\n`))
    console.log(chalk.red(`${err?.message}\n` ?? "Could not connect to node\n"))
    console.log("_________________________________\n")
    const anotherOne = await confirm({ message: 'Try again?', default: false });
    if (anotherOne) { 
      return await setupControlNodes(controlNodes, nodeObj) 
    }
  }
}

async function buildControlNodes({node, nodeObj}) {
  let authObj = {
    host: node.ip,
    cert: node.cert,
    macaroon: node.macaroon,
    // protoDir: path.join(__dirname, "proto")
  }
  return new Promise (async (resolve, reject) => {
    let validatedNode = {}
    try {
      const grpc = new LndGrpc(authObj)
  
      const { Lightning } = grpc.services
      // Do something cool if we detect that the wallet is locked.
      grpc.on(`connected`, () => console.log('wallet connected!'))
      // Do something cool if we detect that the wallet is locked.
      grpc.on(`locked`, async () => {
        await grpc.activateLightning()
      })
  
      // Do something cool when the wallet gets unlocked.
      grpc.on(`active`, async () => {
        const current_node = await Lightning.getInfo();
        const nodeGraph = await Lightning.describeGraph();
  
        if (nodeGraph.nodes < 1) {
          console.log(`Node: ${node.alias} has no graph`)
          throw new Error("Please check that controlled nodes have open channels to other nodes")
        }
        if (current_node) {
          nodeObj[current_node.identity_pubkey] = current_node;
          nodeObj[current_node.identity_pubkey].graph = nodeGraph;
          node.id = current_node.identity_pubkey;

          nodeObj[current_node.identity_pubkey].possible_dests = nodeGraph.nodes.filter((n) => {
            return n.pub_key != current_node.identity_pubkey
          })
          validatedNode = {
            "ip": node.ip,
            "macaroon": node.macaroon,
            "cert": node.cert,
            "id": current_node.identity_pubkey,
            "alias": current_node.alias
          }
        }
        await grpc.disconnect()
      })
      grpc.on(`disconnected`, async() => {
        if (validatedNode.id) {
          resolve(validatedNode)
        } else {
          reject()
        }
      })
      await grpc.connect();
    } catch (err) {
      reject(err ?? "couldn't connect to node")
    }
  })
}

export { setupControlNodes, buildControlNodes };
