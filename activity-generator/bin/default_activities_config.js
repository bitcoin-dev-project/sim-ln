
export const DefaultConfig = {
  "ACTION_TYPE": {
    "KEYSEND_PAYMENTS": "keysend"
  },
  "HIGH_VOLUME_TRANSACTIONS": {
    name: "High Volume Keysend Payments",
    frequency: 5,
    amount: 100000,
    action: "keysend"
  },
  "LOW_VOLUME_TRANSACTIONS": {
    name: "Low Volume Keysend Payments",
    frequency: 5,
    amount: 100,
    action: "keysend"
  },
  "FREQUENCY_OPTIONS": {
    name: "Interval between payments",
    options: [
      {
        name: "High (10 seconds)",
        value: 10,
      },
      {
        name: "Medium (30 minutes)",
        value: 1800,
      },
      {
        name: "Low (1 hour)",
        value: 3600,
      }
    ]
  },
  "AMOUNT_OPTIONS": {
    name: "Amount sent in each transaction",
    options: [
      {
        name: "High (20% of channel capacity)",
        value: 20,
      },
      {
        name: "Medium (10% of channel capacity)",
        value: 10,
      },
      {
        name: "Low (2% of channel capacity)",
        value: 2,
      }
    ]
  },
}