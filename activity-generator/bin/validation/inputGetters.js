import { input } from '@inquirer/prompts';
import { ErrorConfig } from './err_config.js';

const getFrequency = async () => {
  const errMessage = `Frequency ${ErrorConfig.POSITIVE_INTEGER}`
  try {
    const freq = await input({ message: 'Time delay between actions in seconds', default: 1 });
    if (isNaN(parseInt(freq)) || parseInt(freq) < 1) {
      throw new Error(errMessage)
    }
    return parseInt(freq)
  } catch (err) {
    if (err.message === errMessage) {
      console.error(err.message)
      return await getFrequency()
    }
  }
}
const getAmountInSats = async () => {
  const errMessage = `Amount ${ErrorConfig.POSITIVE_INTEGER}`
  try {
    const amt = await input({ message: 'How many sats?', default: 1000 });
    if (isNaN(parseInt(amt)) || parseInt(amt) < 1) {
      throw new Error(errMessage)
    }
    return parseInt(amt)
  } catch (err) {
      console.error(err.message)
      if (err.message === errMessage) {
        return await getAmountInSats()
      }
  }
}

const verifyPubKey = async (allPossibleNodes) => {
  try {
    const pubkey = await input({ message: 'Enter pubkey:' });
    const isValidPubkey = allPossibleNodes.find((_pubkey) => pubkey === _pubkey )
    if (!isValidPubkey) {
      throw new Error(ErrorConfig.PUBKEY_NOT_FOUND)
    }
    return pubkey
  } catch (err) {
    console.error(err.message)
    if (err.message === ErrorConfig.PUBKEY_NOT_FOUND) {
      return await verifyPubKey(allPossibleNodes)
    }
  }
}

export { getFrequency, getAmountInSats, verifyPubKey }